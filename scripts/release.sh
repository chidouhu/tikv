#!/usr/bin/env bash

set -e

error() {
    echo $@ >&2
    return 1
}

if [[ $# -ne 1 ]]; then
    error $1 [hash\|tag\|branch]
fi

if [[ -d tikv ]]; then
    cd tikv
    git pull
else
    git clone https://github.com/pingcap/tikv.git
    cd tikv
fi
git checkout $1

scl enable devtoolset-4 python27 /bin/bash
ROCKSDB_SYS_STATIC=1 ROCKSDB_SYS_PORTABLE=1 make release
