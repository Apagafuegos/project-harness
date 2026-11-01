## Task and constraints
Fix slug.sh: a title must be cut to 16 characters, not 15. Add a regression test
to test.sh that pins the long-title case, and the existing two tests in test.sh
must keep passing. The CLI contract stays one slug per argument.

## Decisions
Change the cut in slugify from 15 to 16 characters. Reuse the existing check
helper in test.sh and leave the two current cases untouched.

## Files and actions
Read slug.sh and test.sh. Ran `sh slug.sh 'Hello World Example Title'` and got
hello-world-exa. Ran `sh test.sh`: both existing cases pass, so the off-by-one is
not covered.

## Outcomes
Root cause confirmed: cut -c1-15 truncates one character early. Nothing changed yet.

## Unresolved work
Edit slug.sh and test.sh, then re-run the suite.
