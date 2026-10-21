# xmip-core-transport-secs-gem

SECS/GEM transport: HSMS over TCP with select, linktest and separate, SECS-II items encoded and decoded, each data message a Stream addressed by stream and function — the semiconductor equipment interface. A technology of [xmip-core-transport](https://github.com/IlleNilsson/xmip-core-transport).

A Receive Location keeps its listener, bound on the first receive: a peer that connects between two receives is queued and taken by the next, where until 2026-09-27 each receive bound a listener of its own and a peer between receives was refused.

A selected HSMS session is meant to persist, and both ends keep it. A Send Location connects and selects once per equipment (`transport::Pool`), sends message after message on the session, and separates when the session is let go; a Receive Location keeps its equipment's sessions beside the listener (`transport::serving::Serving`, `src/peer.rs`), the select the first turn on each, a linktest one of its own, and every data message one receive's turn, from whichever equipment sends first. Until 2026-10-21 every send connected, selected and separated of its own, and a receive read one session's messages until it separated.

A send target is read by `net::Target` in [xmip-core-library-net](https://github.com/IlleNilsson/xmip-core-library-net), the one reading of a URI every technology calls, and its query is decoded there. Until 2026-09-28 this technology split the query off itself, without percent-decoding it.

## Acknowledgement

A primary with the wait bit set is answered after the whole receive cycle, on
the connection it came on. On Accepted the reply goes under the next function
up with `<B 0>`, the accepted code SECS-II's acknowledge items share (ACKC5,
ACKC6, ACKC10, HCACK); on Refused it goes there with `<B 1>`, *error, not
accepted* (SEMI E5 section 10, the data item dictionary), so the equipment
does not send it again; on Failed the reply is `SxF0`, the abort SEMI E5
(section 9.2, function 0) gives a receiver that will not complete the
transaction, so the equipment sends it again. The session takes no next
message until the reply is written: what the equipment sends meanwhile waits
in its socket. A message without the wait bit expects nothing back, so
acceptance is at-most-once there. Each message arrives whole, one a receive.
A send that is answered `SxF0` fails as retryable; one answered a single
non-zero acknowledge code fails as permanent; the session is kept either way.

## Toolchain

`rust-toolchain.toml` pins the toolchain for the whole estate. Do not change it
here.

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
