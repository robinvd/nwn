# notebooks without notebooks

## install

requires rust/cargo this can be done though rustup or os package manager
```
cargo build --release
```
the binary is now in target/release/lsp
point your favorite text editor with lsp support here, or use the [vscode extension](https://github.com/robinvd/nwn-vscode)

the server expect the config files in the folder runtimes to be in ~/.config/nwn/
```
ln -s $full_path_to_here/runtimes ~/.config/nwn
```