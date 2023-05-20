#!/usr/bin/env bash

if [ -z $CHANNEL ]; then
export CHANNEL='debug'
fi

pushd $(dirname "$0") >/dev/null
source config.sh

# read nightly compiler from rust-toolchain file
TOOLCHAIN=$(cat rust-toolchain | grep channel | sed 's/channel = "\(.*\)"/\1/')

popd >/dev/null

if [[ $(rustc -V) != $(rustc +${TOOLCHAIN} -V) ]]; then
    echo "rustc_codegen_gcc is build for $(rustc +${TOOLCHAIN} -V) but the default rustc version is $(rustc -V)."
    echo "Using $(rustc +${TOOLCHAIN} -V)."
fi

cmd=$1
shift

LIBRARY_PATH=/home/bouanto/Ordinateur/Programmation/Projets/gcc-repo/gcc-build/build/gcc GCC_EXEC_PREFIX=/opt/gcc/lib/gcc/x86_64-pc-linux-gnu PATH=$PATH:/opt/gcc/bin RUSTDOCFLAGS="$RUSTFLAGS" cargo +${TOOLCHAIN} $cmd $@
