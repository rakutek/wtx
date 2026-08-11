#!/bin/sh
set -eu

worktree_path=${1:-.}

if [ ! -d "$worktree_path" ]; then
  echo "wtx skill: worktree directory not found: $worktree_path" >&2
  exit 64
fi

cd "$worktree_path"

if [ -e .env ] || [ -L .env ]; then
  echo "wtx skill: preserved existing .env"
  exit 0
fi

if [ ! -f .env.example ]; then
  echo "wtx skill: no .env.example; no .env created"
  exit 0
fi

umask 077
env_tmp=$(mktemp ./.env.wtx.XXXXXX)
cleanup() {
  rm -f "$env_tmp"
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

cp ./.env.example "$env_tmp"
if ln "$env_tmp" ./.env 2>/dev/null; then
  echo "wtx skill: created .env from .env.example"
else
  echo "wtx skill: preserved .env created concurrently"
fi
