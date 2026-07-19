<!--
  Overriding a BUILT-IN template: this file's stem, "help", matches a shipped
  template, so it replaces it. Delete this file to get the shipped text back.

  Telegram HTML is the output format here: <b>, <i>, <code>, <a href>.
  Anything else will be rejected by the Telegram API, so the message would
  fail to send rather than render oddly.

  Placeholders: {{engine}} {{target}} {{session}}
-->
<b>Bridge</b>

Active: <b>{{engine}}</b> on <b>{{target}}</b>

Send anything that is not a command and it goes straight to the active engine.
