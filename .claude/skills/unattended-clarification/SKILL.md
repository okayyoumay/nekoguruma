---
name: unattended-clarification
description: Use when a task hits a genuine ambiguity that needs the user's judgment (CLAUDE.md's "ask before making changes" rule) but the session may be running unattended, for example fired by a schedule, a trigger or a self-set wakeup. Covers recognizing that no live user is present and what to do instead of retrying the question.
---

# Asking a question when no user may be watching

CLAUDE.md asks you to stop and ask when an ambiguity changes the outcome.
`AskUserQuestion` needs an interactive client to render the prompt and wait
for an answer. A session resumed by a schedule, a trigger or a self-set
wakeup has no guarantee that anyone is watching.

## Recognizing it

`AskUserQuestion` fails with an error saying the permission stream closed
before a response was received. That means no live client is attached, not a
network blip. Retry at most once; a second identical failure confirms it.

## What to do instead

1. Do not resolve the ambiguity yourself. An unattended run is not a licence
   to guess.
2. Send the question through a channel that reaches the user later: a
   `PushNotification` if available, and the reply tool of the surface you
   run in (for example a thread reply in a project), with the question
   written out in full.
3. Give enough context to answer without reopening the investigation: what
   you found, why it is ambiguous, and the options, each answerable in a
   word, with your recommendation marked.
4. Carry on with any part of the work that does not depend on the answer.
   Otherwise end the turn without making the change.
5. When the user's answer arrives in a later turn, resume from it.

Do not downgrade "ask the user" to "pick the conservative option and
proceed" because the channel is inconvenient. Reversible defaults that do
not change the outcome are a different case: CLAUDE.md lets you pick those
and say which one you picked.
