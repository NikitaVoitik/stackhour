<!--
  Overriding a BUILT-IN template: this file's stem, "help", matches a shipped
  template, so it replaces it. Delete this file to get the shipped text back.

  Telegram HTML is the output format here: <b>, <i>, <code>, <a href>.
  Anything else will be rejected by the Telegram API, so the message would
  fail to send rather than render oddly.

  Placeholders:
    {{commands}}  one "/usage — description" line per visible command,
                  generated from the command table and HTML-escaped. Using it
                  is the reason to override /help: the list then stays correct
                  as you add commands. The SHIPPED body deliberately does NOT
                  use it, so a user who configures nothing keeps the exact
                  /help the bridge has always had.
    {{engine}}    active engine label
    {{target}}    active target label

  None of the three is required, so `stackhour bridge doctor` will not
  complain if you drop one.
-->
<b>Bridge</b> — {{engine}} on {{target}}

{{commands}}

<i>Send anything that is not a command and it goes straight to the active engine.</i>
