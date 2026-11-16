#!/usr/bin/env python3
"""WO-069 lane F2: a loopback TCP proxy with ONE fault hook, for the item 5 kill drills. Test scaffolding, not product code.

    wo069-gate-hold-proxy.py <listen-port> <upstream-host> <upstream-port> <ctl-dir>

Listens on 127.0.0.1, forwards bytes both ways and logs `<epoch> <METHOD> <path>` of each request line to <ctl-dir>/requests.log
(paths only; never headers, never bodies, so no credential is logged). The hook is armed by a file in <ctl-dir>:

    hold   its text is "<mode> <substring>". mode is
           hold  the first PUT whose path contains the substring is forwarded in full, its RESPONSE is withheld until
                 <ctl-dir>/release exists (or the client connection closes); <ctl-dir>/held is written with the epoch.
                 Used to kill a daemon after the store has applied a write but before the daemon heard of it.
           drop  the first PUT whose path contains the substring is NOT forwarded and the connection is closed;
                 <ctl-dir>/held is written. Used to kill a daemon between two writes of one registry refresh.

The hook fires once (the hold file is removed when it fires). What it cannot do: stop a request that is split over several TCP
reads before its request line (the request line is matched on the first read of a new request), or time a kill inside the store.
"""
import asyncio, os, re, sys, time

LPORT, UHOST, UPORT, CTL = int(sys.argv[1]), sys.argv[2], int(sys.argv[3]), sys.argv[4]
REQ = re.compile(rb'(PUT|POST|GET|HEAD|DELETE) (\S+) HTTP/1\.[01]')
LOG = open(os.path.join(CTL, 'requests.log'), 'a', buffering=1)


def arm():
    p = os.path.join(CTL, 'hold')
    try:
        mode, _, pat = open(p).read().strip().partition(' ')
    except OSError:
        return None
    return (mode, pat) if mode in ('hold', 'drop') and pat else None


async def c2s(r, w, st):
    try:
        while d := await r.read(65536):
            m = REQ.match(d)
            if m:
                meth, path = m.group(1).decode(), m.group(2).decode()
                LOG.write('%.3f %s %s\n' % (time.time(), meth, path.split('?')[0]))
                a = arm()
                if a and meth == 'PUT' and a[1] in path:
                    os.remove(os.path.join(CTL, 'hold'))
                    open(os.path.join(CTL, 'held'), 'w').write('%.3f\n' % time.time())
                    if a[0] == 'drop':
                        return
                    st['hold'] = True
            w.write(d)
            await w.drain()
    except Exception:
        pass
    finally:
        st['closed'] = True
        try:
            w.close()
        except Exception:
            pass


async def s2c(r, w, st):
    try:
        while d := await r.read(65536):
            while st.get('hold') and not st.get('closed') and not os.path.exists(os.path.join(CTL, 'release')):
                await asyncio.sleep(0.02)
            w.write(d)
            await w.drain()
    except Exception:
        pass
    finally:
        try:
            w.close()
        except Exception:
            pass


async def handle(r, w):
    try:
        r2, w2 = await asyncio.open_connection(UHOST, UPORT)
    except Exception:
        w.close()
        return
    st = {}
    await asyncio.gather(c2s(r, w2, st), s2c(r2, w, st))


async def main():
    s = await asyncio.start_server(handle, '127.0.0.1', LPORT)
    await s.serve_forever()


asyncio.run(main())
