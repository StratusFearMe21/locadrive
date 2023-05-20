# LocaDrive

An easy to use, fast, and reliable file transfer program. It works by creating a
web server on your computer which serves a WebAssembly application which can be
used to upload files directly to your computer, or the other way around!

## Features
- Written in pure Rust
- Desktop side powered by `iced` and `axum`
- WASM side powered by `sledgehammer_bindgen` and raw `web-sys`
- Syncronised GUI state between WASM and Desktop sides
- Concurrent uploading between multiple devices
- Uploads have Zstd compression for uncompressed file types
- Transfer from WASM to Desktop side *or* vise-verca
- Transfer multiple files to the WASM side by compressing them into a `.tar` file

## Installation
1. Make sure to clone this repo with submodules
```shell
git clone --recursive https://github.com/StratusFearMe21/locadrive.git
```
2. Compile `locadrive_wasm` using `trunk`
```shell
cd locadrive_wasm
trunk build --release
```
3. Compile `locadrive_iced` using `cargo`
```shell
cd locadrive_iced
cargo build --release
```
4. Make sure that the `dist` folder in `locadrive_wasm` is in your working directory, and launch the program!

## Usage
Launch `locadrive_iced` with the `locadrive_wasm` `dist` folder in your working directory, then follow the instructions in the GUI.