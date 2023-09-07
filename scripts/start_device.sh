#!/usr/bin/env bash


set -eEuo pipefail


p=''

function prompt() {
    read -rp "$1 (y/n) " p

    if [ "$p" == "y" ]; then
        p="y"
    else
        p="n"
    fi
}

function run() {
    local cmd
    cmd=$(echo "$1" | paste -sd' ' | sed 's/[[:space:]]\{2,\}/ /g')

    printf '\n$ %s\n' "$cmd"
    echo "==="
    $cmd
    echo "==="
}

prompt "Commit and push?"
if [ "$p" == "y" ]; then
    run 'git commit --all --amend --no-verify --no-edit'
    run 'git push --force-with-lease'
fi

prompt "Run device?"
if [ "$p" == "y" ]; then
    read -rp "Insert bridge url: " bridge_url # ws://192.168.122.36:8080
    run "cargo r -p app device --bridge-url $bridge_url"
fi
