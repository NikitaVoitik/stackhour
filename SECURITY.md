# Security

This bridge intentionally exposes local coding agents through a Telegram bot. Treat the bot token, allowed Telegram account, SSH key, and both agent sessions as privileged access.

- Keep `config.json`, `worker-config.json`, runtime state, logs, media, and queue directories out of Git. They are ignored by default.
- Restrict both config files and the SSH private key to the owning user (`chmod 600`).
- Use a dedicated Telegram bot and set `chatId` to the only chat allowed to submit work.
- Keep `permissionMode` set to `default`. `bypassPermissions` gives remotely submitted prompts substantially more power and disables normal Codex/Claude safeguards.
- Run the coordinator and worker as unprivileged users. Do not run either service as root.
- Review logs before sharing them; prompts, file paths, and tool activity can be sensitive.
- Rotate the bot token and SSH key immediately if either is exposed.

Please report vulnerabilities privately to the repository owner rather than opening a public issue containing exploit details or credentials.
