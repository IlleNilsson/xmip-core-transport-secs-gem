# xmip-core-transport-secs-gem

SECS/GEM transport: HSMS over TCP with select, linktest and separate, SECS-II items encoded and decoded, each data message a Stream addressed by stream and function — the semiconductor equipment interface. A technology of [xmip-core-transport](https://github.com/IlleNilsson/xmip-core-transport).

A Receive Location keeps its listener, bound on the first receive (`transport::kept::Kept`): a peer that connects between two receives is queued and taken by the next, where until 2026-09-27 each receive bound a listener of its own and a peer between receives was refused.

A send target is read by `net::Target` in [xmip-core-library-net](https://github.com/IlleNilsson/xmip-core-library-net), the one reading of a URI every technology calls, and its query is decoded there. Until 2026-09-28 this technology split the query off itself, without percent-decoding it.

## Acknowledgement

A primary with the wait bit set is answered after the whole receive cycle, on
the connection it came on. On Accepted the reply goes under the next function
up with `<B 0>`, the accepted code SECS-II's acknowledge items share (ACKC5,
ACKC6, ACKC10, HCACK); on Refused it goes there with `<B 1>`, *error, not
accepted* (SEMI E5 section 10, the data item dictionary), so the equipment
does not send it again; on Failed the reply is `SxF0`, the abort SEMI E5
(section 9.2, function 0) gives a receiver that will not complete the
transaction, so the equipment sends it again. A receive stops at such a primary and keeps the connection; the next
receive goes on reading it once the reply is written. A message without the
wait bit expects nothing back, so acceptance is at-most-once there. Each
message arrives whole. A send that is answered `SxF0` fails as retryable; one
answered a single non-zero acknowledge code fails as permanent.

A receive takes every message a connection carries, whatever room the runtime
gives it (`Transport::receive_up_to`): one without the wait bit was let go
of as it was sent, and none can wait at the far end.

## Toolchain

`rust-toolchain.toml` pins the toolchain for the whole estate. Do not change it
here.

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
