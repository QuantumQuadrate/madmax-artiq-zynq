#!/usr/bin/env bash

# Only ZC706 supported for now.

set -e

if [ -z "$OPENOCD_ZYNQ" ]; then
    echo "OPENOCD_ZYNQ environment variable must be set"
    exit 1
fi
if [ -z "$SZL" ]; then
    echo "SZL environment variable must be set"
    exit 1
fi

target_host="rpi-4"
impure=0
pure_dir="result"
impure_dir="build"
sshopts=""
load_bitstream=1
board_host="192.168.1.52"
fw_type="runtime"

while getopts "h:id:o:lt:" opt; do
    case "$opt" in
    \?) exit 1
        ;;
    h)  target_host=$OPTARG
        ;;
    i)  impure=1
        ;;
    d)  pure_dir=$OPTARG;
        impure_dir=$OPTARG;
        ;;
    o)  sshopts=$OPTARG
        ;;
    l)  load_bitstream=0
        ;;
    b)  board_host=$OPTARG
        ;;
    t)  fw_type=$OPTARG
        ;;
    esac
done

target_folder="/tmp/zynq-$USER"
load_bitstream_cmd=""

echo "Creating $target_folder..."
ssh $sshopts $target_host "mkdir -p $target_folder"
echo "Copying files..."
rsync -e "ssh $sshopts" -Lc $OPENOCD_ZYNQ/* $target_host:$target_folder
rsync -e "ssh $sshopts" -Lc $SZL/szl-zc706.elf $target_host:$target_folder/szl.elf
if [ $impure -eq 1 ]; then
    if [ $load_bitstream -eq 1 ]; then
        load_bitstream_cmd="-g build/gateware/top.bit"
    fi
    firmware="build/$fw_type.bin"
else
    if [ $load_bitstream -eq 1 ]; then
        load_bitstream_cmd="-g $pure_dir/top.bit"
    fi
    firmware="$pure_dir/$fw_type.bin"    
fi
echo "Programming board..."
ssh $sshopts $target_host "cd $target_folder; openocd -f zc706.cfg -c'load_image szl.elf; resume 0; exit'"
sleep 5
artiq_netboot $load_bitstream_cmd -f $firmware -b $board_host
