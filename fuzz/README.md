# Fuzzing

The Prometheus parser is dependency-free and included directly by the fuzz
target, so a libFuzzer build does not need a library-crate refactor.

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz run parse_prometheus
```

The same malformed-input coverage also runs in the normal gate as
`parser_mutation_fuzz_smoke_does_not_panic`.
