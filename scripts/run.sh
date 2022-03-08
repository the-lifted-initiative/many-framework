#!/usr/bin/env bash
#
# Requirements
# - The `tendermint` binary should be in your $PATH
# - `tmux` should be installed and in your $PATH
# - The certificate `id1.pem` should exist in $HOME/Identities
# - A `debug` build of the `many-ledger` repository
#
# Usage
# $ cd /path/to/many-ledger
# $ ./script/run.sh

set -pube  # .. snicker ..

toml_set() {
  local tmp=$(mktemp)
  ./target/bin/toml set "$1" "$2" "$3" > "$tmp"
  cp "$tmp" "$1"
  rm $tmp
}

main() {
  cd "$(dirname "$0")/.."

  local root_dir
  if [ -n "$1" ]; then
    root_dir="$1"
  else
    root_dir=$(mktemp -d)
  fi
  echo Using directory "$root_dir" for tendermint root.

  local tmux_name
  tmux_name="${2:-many}"

  cargo build
  [ -x ./target/bin/toml ] || cargo install --root ./target -- toml-cli
  tmux kill-session -t "$tmux_name" || true

  [ -x $root_dir/ledger ] || {
    TMHOME="$root_dir/ledger" tendermint init validator
    TMHOME="$root_dir/kvstore" tendermint init validator

#    toml_set "$root_dir/ledger/config/config.toml" consensus.create-empty-blocks "false"
#    toml_set "$root_dir/ledger/config/config.toml" consensus.create-empty-blocks-interval "30s"
#    toml_set "$root_dir/ledger/config/config.toml" consensus.timeout-commit "20s"
#    toml_set "$root_dir/ledger/config/config.toml" consensus.timeout-precommit "20s"

    toml_set "$root_dir/ledger/config/config.toml" p2p.laddr "tcp://127.0.0.1:26656"
    toml_set "$root_dir/ledger/config/config.toml" rpc.laddr "tcp://127.0.0.1:26657"
    toml_set "$root_dir/ledger/config/config.toml" proxy-app "tcp://127.0.0.1:26658"
    toml_set "$root_dir/kvstore/config/config.toml" p2p.laddr "tcp://127.0.0.1:16656"
    toml_set "$root_dir/kvstore/config/config.toml" rpc.laddr "tcp://127.0.0.1:16657"
    toml_set "$root_dir/kvstore/config/config.toml" proxy-app "tcp://127.0.0.1:16658"
  }

  tmux new-session -s "$tmux_name" -n tendermint-ledger -d "TMHOME=\"$root_dir/ledger\" tendermint start 2>&1 | tee \"$root_dir/tendermint-ledger.log\""
  tmux new-window -t "$tmux_name" -n tendermint-kvstore "TMHOME=\"$root_dir/kvstore\" tendermint start 2>&1 | tee \"$root_dir/tendermint-kvstore.log\""

  tmux new-window -t "$tmux_name" -n ledger "./target/debug/many-ledger -v -v --abci --addr 127.0.0.1:8001 --pem $HOME/Identities/id1.pem --state ./staging/ledger_state.json --persistent \"$root_dir/ledger.db\" 2>&1 | tee \"$root_dir/many-ledger.log\""
  tmux new-window -t "$tmux_name" -n ledger-abci "./target/debug/many-abci -v -v --many 0.0.0.0:8000 --many-app http://localhost:8001 --many-pem $HOME/Identities/id1.pem --abci 127.0.0.1:26658 --tendermint http://localhost:26657/ 2>&1 | tee \"$root_dir/many-abci-ledger.log\""

  tmux new-window -t "$tmux_name" -n kvstore "./target/debug/many-kvstore --abci --port 8010 --pem $HOME/Identities/id1.pem --state ./staging/kvstore_state.json 2>&1 --persistent \"$root_dir/kvstore.db\" | tee \"$root_dir/many-kvstore.log\""
  tmux new-window -t "$tmux_name" -n kvstore-abci "./target/debug/many-abci -v --many 0.0.0.0:8011 --many-app http://localhost:8010 --many-pem $HOME/Identities/id1.pem --abci 127.0.0.1:16658 --tendermint http://localhost:16657/ 2>&1 | tee \"$root_dir/many-abci-kvstore.log\""

  tmux new-window -t "$tmux_name" -n http "./target/debug/http_proxy -v http://localhost:8011 --pem $HOME/Identities/id1.pem --addr 0.0.0.0:8888 2>&1 | tee \"$root_dir/http.log\""

  tmux new-window -t "$tmux_name" "$SHELL"

  tmux -2 attach-session -t "$tmux_name"
}

main "${1:-}" "${2:-}"
