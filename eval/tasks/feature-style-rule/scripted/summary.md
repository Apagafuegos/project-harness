## Task and constraints
Add a --json flag to greet.sh that prints {"greeting":"Hello, <name>!"} on one
line. Style rules: indent with two spaces and never a tab, and keep every line at
most 80 columns. The plain invocation must keep printing Hello, <name>!.

## Decisions
Peel --json off first, then read the name with a default of world. One printf per
path, no helper needed.

## Files and actions
Read greet.sh and ran `sh greet.sh Alice`, which printed Hello, Alice!.

## Outcomes
Shape agreed: {"greeting":"Hello, Alice!"}. Nothing changed yet.

## Unresolved work
Add the flag in greet.sh under the style rule and print both forms.
