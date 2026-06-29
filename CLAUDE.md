# Naru — guidance for Claude

## Always validate after changes
After editing `config.kdl`, the naru codebase, or anything that affects config parsing, run:

```
naru validate
```

The fastest path is the installed binary on `$PATH`. If you've changed Rust source and need to test the new behavior, build first (`cargo build`) and run the freshly built binary; otherwise the system `naru` is fine.

If validation fails, fix the errors before reporting the task complete. Don't claim success on a config that won't load.

## All tests must pass — fix them, don't leave them red
After any change, every test must pass. If a test fails — whether it's one you
just wrote, a pre-existing one, or someone else's — fix it, without hesitation
and without asking for approval. Do not leave failing tests, and do not report a
task complete with known-failing tests.

Default to fixing the **code** so the test passes. Only change a test when the
behavior it checks has *genuinely, intentionally* changed — i.e. the test now
asserts something functionally different from what the code is supposed to do. In
that case **adapt** the test to the new expected behavior; prefer adapting over
removing it (remove only when the test no longer has any valid purpose). Never
weaken or delete a test merely to make a suite go green, and never accept a new
snapshot value without first confirming the new behavior is actually correct
(a wrong value can hide a real regression).
