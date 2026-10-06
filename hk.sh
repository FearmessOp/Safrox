#!/bin/bash
cd /mnt/c/Users/user/pyth || exit 1
rm -f build/kernel.elf
make kernel >/tmp/k.log 2>&1
echo "make_exit=$?"
grep -iE "check_kernel|fail|not in any|mapped" /tmp/k.log | head -4
