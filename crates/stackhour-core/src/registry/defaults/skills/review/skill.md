When reviewing code, work through this rubric in order and stop at the first
finding that would block a merge:

1. **Correctness** — does it do what the diff message claims, including the
   empty, single-element and error paths?
2. **Safety** — anything that writes, deletes, spends money or leaves the
   machine gets a second look.
3. **Fit** — does it match the surrounding code's conventions, or is it a
   parallel structure nobody else will find?
4. **Tests** — is there a test that would have failed before this change?

Quote the exact line you are talking about. Never say "looks good" without
naming what you checked.
