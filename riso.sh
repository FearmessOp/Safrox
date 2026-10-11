#!/bin/bash
source ~/.cargo/env 2>/dev/null
cd /mnt/c/Users/user/pyth || exit 1
for i in 1 2 3; do
  echo "=== KOSU $i ===" >> /mnt/c/Users/user/pyth/iso.log
  make run-kbdisolation >> /mnt/c/Users/user/pyth/iso.log 2>&1
  echo "EXIT=$?" >> /mnt/c/Users/user/pyth/iso.log
done
