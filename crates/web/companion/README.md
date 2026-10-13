# amem companion

A browser extension beside the amem page. A session's `fetch` whose grant says
`browser = true` is made here, by this browser, with its own logins: a page you are signed
in to answers the session as it would answer you, and no credential is copied into a grant.

Install, unpacked: `chrome://extensions`, Developer mode, Load unpacked, this directory.
Then open the amem page (`amem` opens it paired); the page hands the extension its key,
and shows the companion as connected.

What holds. An ask comes only from amem's own launcher, with the token a page never
holds. The memory server decided the URL by the session's policy before asking, and the
ask names the hosts the session may reach at its label; the extension goes nowhere else,
and a redirect elsewhere is reported, not read. The URL that answered is decided again
before anything is kept, the answer is scanned as every tool's is, and it is kept as a
source observed by `browser`. See PLAN M19, third cut.
