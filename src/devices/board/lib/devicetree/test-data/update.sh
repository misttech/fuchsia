#!/bin/sh
# TODO(b/242641399): These should be built using dtc at compile time.

for i in *.dts; do
	dtc $i > $(basename -s .dts $i).dtb
done
