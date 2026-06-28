alias r := run
alias b := build
alias rel := release
alias c := check
alias t := test
alias l := lint
alias f := fmt

run file="":
    cargo run {{file}}

build:
    cargo build

release:
    cargo build --release

check:
    cargo check

test:
    cargo test

fmt:
    cargo fmt

lint:
    cargo clippy -- -D warnings

clean:
    cargo clean
