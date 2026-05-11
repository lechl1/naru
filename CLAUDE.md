# Naru — guidance for Claude

## Always validate after changes
After editing `config.kdl`, the naru codebase, or anything that affects config parsing, run:

```
naru validate
```

The fastest path is the installed binary on `$PATH`. If you've changed Rust source and need to test the new behavior, build first (`cargo build`) and run the freshly built binary; otherwise the system `naru` is fine.

If validation fails, fix the errors before reporting the task complete. Don't claim success on a config that won't load.
