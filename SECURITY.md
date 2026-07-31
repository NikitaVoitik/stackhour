# Security

Stackhour can run local coding agents from authenticated web and Telegram
clients. Treat client tokens, node tokens, bot tokens, SSH keys, and provider
sessions as privileged credentials.

- Keep `config.json`, runtime state, logs, and databases out of Git.
- Restrict configuration and SSH private keys to the owning user (`chmod 600`).
- Use a dedicated Telegram bot and allow only the configured chat.
- Run hub and node services as unprivileged users.
- Put TLS in front of the hub whenever traffic crosses an untrusted network.
- Keep the hub bound to `127.0.0.1` when it is behind a reverse proxy.
- Review logs before sharing them; prompts, paths, and tool activity can be
  sensitive.
- Treat `full_access` as equivalent to provider bypass mode: it maps to
  `--dangerously-bypass-approvals-and-sandbox` for Codex and
  `bypassPermissions` for Claude. Use it only for explicitly trusted
  workspaces and prompts.
- Rotate any token or key immediately after exposure.

Please report vulnerabilities privately to the repository owner rather than
opening a public issue containing exploit details or credentials.
