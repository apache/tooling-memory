# amem companion

A browser extension beside the amem page. A session's `fetch` whose grant says
`browser = true` is made here, by this browser, with its own logins: a page you are signed
in to answers the session as it would answer you, and no credential is copied into a grant.

Install, unpacked: `chrome://extensions`, Developer mode, Load unpacked, this directory.
Then open the amem page (`amem` opens it paired); the page hands the extension its key,
and shows the companion as connected.

What holds. An ask comes only from amem's own launcher, with the token a page never
holds. The memory server decided the URL by the session's policy before asking, and the
ask names the hosts the session may reach at its label; the extension goes nowhere else.
While an ask is in flight, the browser's own request rules (`declarativeNetRequest`, on
this extension as the initiator and on nothing else in the browser) block every request
of the extension's to a host the ask does not name, so a redirect elsewhere is stopped
before it is made, and the refusal says where it was going. The URL that answered is
decided again before anything is kept, the answer is scanned as every tool's is, and it
is kept as a source observed by `browser`. See PLAN M19, third cut.

Rendering. A fetch with `render` is opened in a tab of this browser instead, not shown,
and read as its scripts built it once the page holds still; the tab's navigation, top
frame and frames in it, is held to the same hosts by the same rules, and the tab is
closed when read. What the page itself loads, scripts and images, is the page's own, as
it is when you open it.

Permissions, and why: `storage` and `alarms` keep the pairing and reconnect the socket;
`scripting` reaches an amem page already open and reads a rendered tab; `tabs` opens and
watches that tab; `declarativeNetRequest` holds requests to the hosts; `webRequest`
reads, never blocks, where a stopped request was going. Host permissions on http and
https are what lets a fetch carry your cookies to a host an ask names.
