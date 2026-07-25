# Render-parity fixtures (frozen)

`cases.json` is the input corpus and `golden.json` the recorded output of the
retired Node coordinator's message renderer, payload for payload.
`crates/stackhour-bridge/tests/rendering_parity_with_the_node.rs` replays the
corpus through `deliver_final` and asserts the Rust renderer produces the same
Bot API calls, call-for-call and byte-for-byte.

**These captures cannot be regenerated.** They were produced by a Node harness
that read the live `coordinator.mjs` as text, sliced out its rendering section,
and evaluated it with only `tg()` stubbed. Node has been removed from the
project, and the generator scripts (`gen-cases.mjs`, `node-reference.mjs`)
went with it.

Treat the golden as the specification. If the parity test fails, the Rust
renderer's behaviour changed, and the golden is the evidence of what it used to
be. Only edit these files as part of a deliberate, documented rendering change
— and change `cases.json` and `golden.json` together, since the test asserts
they stay the same length.

`drive-release-binary.py` is unrelated to regeneration: it drives the release
`stackhour` binary against a local mock Bot API for one named case, to inspect
what the current renderer actually emits. It needs no Node.
