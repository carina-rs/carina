---
title: completions
---

Generate shell-completion scripts for the `carina` CLI.

## Usage

```bash
carina completions <SHELL>
```

Supported shells: `bash`, `zsh`, `fish`, `elvish`, `powershell`.

The generated script is written to stdout — redirect it into the location your shell expects.

## Examples

### Bash

```bash
carina completions bash > ~/.local/share/bash-completion/completions/carina
```

### Zsh

```bash
mkdir -p ~/.zfunc
carina completions zsh > ~/.zfunc/_carina
# Add to .zshrc, then start a new shell:
#   fpath=(~/.zfunc $fpath)
#   autoload -Uz compinit; compinit
```

### Fish

```bash
carina completions fish > ~/.config/fish/completions/carina.fish
```
