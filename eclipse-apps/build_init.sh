#!/bin/bash
echo "Construyendo Eclipse Init System..."
mkdir -p /tmp/eclipse-init
cd init
cargo build --release
if [ $? -eq 0 ]; then
    echo "✓ Init compilado correctamente"
    cp target/release/eclipse-init /tmp/eclipse-init/sbin/init
else
    echo "✗ Error al compilar init"
    exit 1
fi
mkdir -p /tmp/eclipse-init/{bin,sbin,etc,var,home,root,proc,sys,dev,tmp}
cp ../etc/eclipse/init.json /tmp/eclipse-init/etc/eclipse/
echo "✓ Eclipse Init System construido en /tmp/eclipse-init/"
