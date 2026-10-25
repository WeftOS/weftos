#!/bin/bash
# Restart GenieX serve on the Ventuno (loopback only) with the given context size.
# Usage: VENQ_HOST=user@board serve.sh <nctx>
NCTX=${1:?nctx}
ssh "${VENQ_HOST:-arduino@ventunoq}" "export PATH=\$HOME/.local/bin:\$PATH; P=\$(pgrep -x geniex); [ -n \"\$P\" ] && kill \$P; sleep 2; setsid nohup geniex serve --host 127.0.0.1:18181 --compute npu --nctx $NCTX --keepalive 30 > /data/build/geniex-serve.log 2>&1 < /dev/null & sleep 5"
if curl -s -m 10 http://127.0.0.1:18181/v1/models >/dev/null; then echo "serve up nctx=$NCTX"; else echo "serve DOWN"; exit 1; fi
