//! Generic cycle detection for the three composition graphs in the registry:
//! agents via `extends`, skills via `uses`, commands via `steps`.
//!
//! One implementation, three call sites. The algorithm is an ITERATIVE
//! depth-first search with a three-colour map (white = unvisited, grey = on
//! the current stack, black = finished). Iterative rather than recursive
//! because the input is user-authored config: a 10k-deep `extends` chain must
//! produce an error, not a stack overflow. A hard [`MAX_DEPTH`] cap gives the
//! same guarantee for pathological-but-acyclic chains.
//!
//! Guarantees relied on by the loader:
//! - Nodes are visited in the caller's iteration order and each cycle is
//!   reported ONCE, as the full path with the entry node repeated at the end
//!   (`["a", "b", "a"]`), so the message reads `cycle a -> b -> a`.
//! - Edges to unknown nodes are IGNORED here — a dangling reference is a
//!   cross-reference error, reported separately with a much better message.
//! - The function never panics and never allocates unboundedly.

use indexmap::{IndexMap, IndexSet};

use super::error::FieldError;

/// Hard cap on composition depth. A chain longer than this is reported as a
/// depth error and its tail is dropped, so a pathological config cannot make
/// the loader do unbounded work.
pub const MAX_DEPTH: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Colour {
    White,
    Grey,
    Black,
}

/// Find every cycle reachable from `nodes`, following `edges_of`.
///
/// Returns one path per distinct cycle, each of the form
/// `[n0, n1, ..., n0]`. `nodes` is the authoritative node set: an edge naming
/// something not in `nodes` is ignored.
pub fn detect_cycles<'a, I, F>(nodes: I, edges_of: F) -> Vec<Vec<String>>
where
    I: IntoIterator<Item = &'a str>,
    F: Fn(&str) -> Vec<String>,
{
    let known: IndexSet<String> = nodes.into_iter().map(str::to_string).collect();
    let mut colour: IndexMap<String, Colour> = known.iter().map(|n| (n.clone(), Colour::White)).collect();
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let mut seen_cycles: IndexSet<String> = IndexSet::new();

    for root in known.iter() {
        if colour.get(root) != Some(&Colour::White) {
            continue;
        }
        // Explicit stack of (node, remaining edges) frames.
        let mut stack: Vec<(String, std::vec::IntoIter<String>)> = Vec::new();
        let mut path: Vec<String> = Vec::new();

        colour.insert(root.clone(), Colour::Grey);
        path.push(root.clone());
        stack.push((root.clone(), filtered_edges(&known, &edges_of, root)));

        while let Some((_, edges)) = stack.last_mut() {
            match edges.next() {
                Some(next) => {
                    match colour.get(&next).copied().unwrap_or(Colour::Black) {
                        Colour::Grey => {
                            // Back edge: the cycle is the tail of `path`
                            // starting at `next`, closed by `next` again.
                            if let Some(at) = path.iter().position(|n| *n == next) {
                                let mut cycle: Vec<String> = path[at..].to_vec();
                                cycle.push(next.clone());
                                if seen_cycles.insert(canonical_key(&cycle)) {
                                    cycles.push(cycle);
                                }
                            }
                        }
                        Colour::Black => {}
                        Colour::White => {
                            if path.len() >= MAX_DEPTH {
                                // Refuse to descend further; the depth error
                                // is raised by `check`, not here.
                                colour.insert(next.clone(), Colour::Black);
                                continue;
                            }
                            colour.insert(next.clone(), Colour::Grey);
                            path.push(next.clone());
                            let frame = filtered_edges(&known, &edges_of, &next);
                            stack.push((next, frame));
                        }
                    }
                }
                None => {
                    if let Some((done, _)) = stack.pop() {
                        colour.insert(done, Colour::Black);
                        path.pop();
                    }
                }
            }
        }
    }
    cycles
}

/// The composition depth of `node` following `edges_of`, capped at
/// [`MAX_DEPTH`] + 1. Cycles terminate the walk (they are reported
/// separately), so this always returns.
pub fn depth_of<F>(known: &IndexSet<String>, edges_of: &F, node: &str) -> usize
where
    F: Fn(&str) -> Vec<String>,
{
    let mut seen: IndexSet<String> = IndexSet::new();
    let mut frontier: Vec<String> = vec![node.to_string()];
    let mut depth = 0usize;
    seen.insert(node.to_string());
    while !frontier.is_empty() && depth <= MAX_DEPTH {
        let mut next: Vec<String> = Vec::new();
        for n in std::mem::take(&mut frontier) {
            for e in edges_of(&n) {
                if known.contains(&e) && seen.insert(e.clone()) {
                    next.push(e);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
        depth += 1;
    }
    depth
}

/// Run both checks (cycles + depth cap) over an entity map and return the
/// names that must be DROPPED together with the error to report for each.
///
/// `label` is the entity plural used in the message ("agents", "skills",
/// "commands"); `key` is the manifest key that forms the edge ("extends",
/// "uses", "steps"), so the message names the offending key as required.
pub fn check<T, F>(
    entries: &IndexMap<String, T>,
    edges_of: F,
    label: &str,
    key: &str,
) -> Vec<(String, FieldError)>
where
    F: Fn(&T) -> Vec<String>,
{
    let known: IndexSet<String> = entries.keys().cloned().collect();
    let edge_fn = |name: &str| -> Vec<String> { entries.get(name).map(&edges_of).unwrap_or_default() };

    let mut out: Vec<(String, FieldError)> = Vec::new();
    let mut dropped: IndexSet<String> = IndexSet::new();

    for cycle in detect_cycles(known.iter().map(String::as_str), edge_fn) {
        let path = cycle.join(" -> ");
        // Every node ON the cycle is dropped (the last element repeats the
        // first, so skip it).
        for name in cycle.iter().take(cycle.len().saturating_sub(1)) {
            if dropped.insert(name.clone()) {
                out.push((
                    name.clone(),
                    FieldError::key(key, format!("{label}: cycle {path}")),
                ));
            }
        }
    }

    for name in known.iter() {
        if dropped.contains(name) {
            continue;
        }
        if depth_of(&known, &edge_fn, name) > MAX_DEPTH {
            out.push((
                name.clone(),
                FieldError::key(
                    key,
                    format!("{label}: composition chain deeper than {MAX_DEPTH} levels"),
                ),
            ));
        }
    }
    out
}

fn filtered_edges<F>(known: &IndexSet<String>, edges_of: &F, node: &str) -> std::vec::IntoIter<String>
where
    F: Fn(&str) -> Vec<String>,
{
    edges_of(node)
        .into_iter()
        .filter(|e| known.contains(e))
        .collect::<Vec<_>>()
        .into_iter()
}

/// A rotation-independent key for a cycle, so `a -> b -> a` and
/// `b -> a -> b` are recognised as the SAME cycle and reported once.
fn canonical_key(cycle: &[String]) -> String {
    let body = &cycle[..cycle.len().saturating_sub(1)];
    if body.is_empty() {
        return String::new();
    }
    let min = body
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let mut rotated: Vec<&str> = Vec::with_capacity(body.len());
    for i in 0..body.len() {
        rotated.push(body[(min + i) % body.len()].as_str());
    }
    rotated.join("\u{0}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(pairs: &[(&str, &[&str])]) -> IndexMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(n, e)| (n.to_string(), e.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
            .collect()
    }

    fn cycles_of(g: &IndexMap<String, Vec<String>>) -> Vec<Vec<String>> {
        detect_cycles(g.keys().map(String::as_str), |n| {
            g.get(n).cloned().unwrap_or_default()
        })
    }

    #[test]
    fn acyclic_chain_has_no_cycles() {
        let g = graph(&[("a", &["b"]), ("b", &["c"]), ("c", &[])]);
        assert!(cycles_of(&g).is_empty());
    }

    #[test]
    fn self_loop_is_a_cycle() {
        let g = graph(&[("a", &["a"])]);
        assert_eq!(cycles_of(&g), vec![vec!["a".to_string(), "a".to_string()]]);
    }

    #[test]
    fn two_node_cycle_reports_the_full_path() {
        let g = graph(&[("a", &["b"]), ("b", &["a"])]);
        let c = cycles_of(&g);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].join(" -> "), "a -> b -> a");
    }

    #[test]
    fn longer_cycle_reports_every_hop_in_order() {
        let g = graph(&[("a", &["b"]), ("b", &["c"]), ("c", &["a"])]);
        assert_eq!(cycles_of(&g)[0].join(" -> "), "a -> b -> c -> a");
    }

    #[test]
    fn the_same_cycle_is_reported_once_regardless_of_entry_point() {
        // Two roots both lead into the same b <-> c cycle.
        let g = graph(&[("a", &["b"]), ("b", &["c"]), ("c", &["b"]), ("d", &["c"])]);
        let c = cycles_of(&g);
        assert_eq!(c.len(), 1, "got {c:?}");
        assert_eq!(c[0].join(" -> "), "b -> c -> b");
    }

    #[test]
    fn two_independent_cycles_are_both_reported() {
        let g = graph(&[("a", &["b"]), ("b", &["a"]), ("x", &["y"]), ("y", &["x"])]);
        let mut paths: Vec<String> = cycles_of(&g).iter().map(|c| c.join(" -> ")).collect();
        paths.sort();
        assert_eq!(paths, vec!["a -> b -> a", "x -> y -> x"]);
    }

    #[test]
    fn edges_to_unknown_nodes_are_ignored() {
        // A dangling reference is a cross-reference error, not a cycle.
        let g = graph(&[("a", &["ghost"])]);
        assert!(cycles_of(&g).is_empty());
    }

    #[test]
    fn diamond_without_a_cycle_is_clean() {
        let g = graph(&[("a", &["b", "c"]), ("b", &["d"]), ("c", &["d"]), ("d", &[])]);
        assert!(cycles_of(&g).is_empty());
    }

    #[test]
    fn a_very_long_chain_terminates_without_overflowing_the_stack() {
        let names: Vec<String> = (0..5000).map(|i| format!("n{i}")).collect();
        let g: IndexMap<String, Vec<String>> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let edges = if i + 1 < names.len() {
                    vec![names[i + 1].clone()]
                } else {
                    Vec::new()
                };
                (n.clone(), edges)
            })
            .collect();
        assert!(cycles_of(&g).is_empty());
    }

    // ---- check(): drop lists + FieldError messages ----

    #[test]
    fn check_drops_every_node_on_the_cycle_with_one_error_each() {
        let g = graph(&[("a", &["b"]), ("b", &["a"]), ("ok", &[])]);
        let out = check(&g, |e: &Vec<String>| e.clone(), "agents", "extends");
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(
            out[0].1.clone().in_file("agents/a/agent.toml").to_string(),
            "agents/a/agent.toml: key `extends`: agents: cycle a -> b -> a"
        );
        // Both nodes carry the SAME path, so the message reads identically.
        assert_eq!(out[0].1.msg, out[1].1.msg);
    }

    #[test]
    fn check_is_clean_for_an_acyclic_graph() {
        let g = graph(&[("a", &["b"]), ("b", &[])]);
        assert!(check(&g, |e: &Vec<String>| e.clone(), "skills", "uses").is_empty());
    }

    #[test]
    fn check_enforces_the_depth_cap() {
        let n = MAX_DEPTH + 3;
        let names: Vec<String> = (0..n).map(|i| format!("a{i:02}")).collect();
        let g: IndexMap<String, Vec<String>> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let edges = if i + 1 < n {
                    vec![names[i + 1].clone()]
                } else {
                    Vec::new()
                };
                (name.clone(), edges)
            })
            .collect();
        let out = check(&g, |e: &Vec<String>| e.clone(), "agents", "extends");
        assert!(!out.is_empty());
        assert!(
            out[0].1.msg.contains("deeper than 16 levels"),
            "got {}",
            out[0].1.msg
        );
        // Only the nodes whose own chain exceeds the cap are dropped; the
        // tail of the chain stays loadable.
        assert!(out.iter().all(|(n, _)| n.as_str() < "a03"), "got {out:?}");
    }

    #[test]
    fn check_names_the_edge_key_it_was_given() {
        let g = graph(&[("x", &["x"])]);
        let out = check(&g, |e: &Vec<String>| e.clone(), "commands", "steps");
        assert_eq!(out[0].1.key, "steps");
        assert_eq!(out[0].1.msg, "commands: cycle x -> x");
    }
}
