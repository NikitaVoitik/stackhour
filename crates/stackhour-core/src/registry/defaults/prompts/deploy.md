<!--
  A NEW prompt template (no built-in is named "deploy"), referenced by
  commands/deploy.toml. Placeholders available here:
    {{env}}   {{note}}   — one per [[args]] entry in the command
    {{args}}             — the raw argument string, untouched
  Unknown placeholders are left verbatim rather than blanked, so a typo is
  visible in the output instead of silently disappearing.
-->
Deploy to **{{env}}**.

Work through this in order and stop if any step fails:

1. Confirm the working tree is clean and on the expected branch.
2. Run the test suite. Do not continue on a failure.
3. Deploy to {{env}}.
4. Verify the deploy by hitting the health endpoint, not by reading logs.

Extra instructions from the operator: {{note}}
