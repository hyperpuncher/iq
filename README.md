# iq

interactive jq repl - live filter evaluation with syntax highlighting and auto completion.

built on [qj](https://github.com/6/qj) - the fast jq-compatible filter engine.

## usage

```
iq file.json
cat data.json | iq
```

## options

```
-i, --indent <INDENT>  indent size [default: 4]
-h, --help             print help
-V, --version          print version
```

## install

```
cargo install --git https://github.com/hyperpuncher/iq
```

or grab a binary from the [latest release](https://github.com/hyperpuncher/iq/releases/latest).

## keys

| key             | action                                 |
| --------------- | -------------------------------------- |
| tab             | cycle completion (popup opens on type) |
| enter           | accept popup, else push history        |
| up/down         | cycle popup / history (empty input)    |
| left/right      | move cursor                            |
| home/end        | cursor to line edges                   |
| backspace/del   | delete left/right char                 |
| pgup/pgdn       | page scroll                            |
| mouse wheel     | scroll                                 |
| ctrl+c / ctrl+d | quit                                   |
| esc             | close popup, else quit                 |
| f1              | debug overlay                          |

## build

```
just build
just run file.json
```

## license

MIT
