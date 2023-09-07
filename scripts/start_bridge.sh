#!/usr/bin/env bash

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

prompt "Pull changes?"
if [ "$p" == "y" ]; then
    run 'git pull --rebase'
fi

prompt "Run bridge?"
if [ "$p" == "y" ]; then
    read -rp "Insert listener address: " listener_addr # 0.0.0.0:8080
    read -rp "Insert browser address: " browser_addr   # 127.0.0.1:9090

    run "cargo r -p app bridge --listener-addr $listener_addr --browser-addr $browser_addr"
fi
