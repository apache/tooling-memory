# amem companion

This is a browser extension that works beside the amem page. When a session calls
`fetch`, and the grant of `fetch` says `browser = true`, this extension makes the request.
It makes it in this browser, with the logins of this browser. Thus a page that you are
signed in to gives the session the same answer that it gives you. Nobody copies a
credential into a grant.

To install the extension unpacked:

1. Open `chrome://extensions`.
2. Set Developer mode.
3. Click Load unpacked, and select this directory.
4. Open the amem page. `amem` opens it already paired.

The page gives the extension its key, and shows that the companion is connected.

Guarantees. Only the launcher of amem sends an ask. The ask has a token that a page never
has. Before the ask, the memory server decided the URL with the policy of the session. The
ask names the hosts that the session may reach at its label. The extension goes to no
other host. While an ask is in progress, the request rules of the browser
(`declarativeNetRequest`) apply. They block each request from the extension to a host that
the ask does not name. These rules apply to this extension as the initiator, and to
nothing else in the browser. Thus the rules stop a redirect to a different host before the
browser makes the request. The refusal says where the redirect was going. Before anything
goes into the record, the policy decides again on the URL that answered. The scan checks
the answer, as it checks the answer of each tool. The record keeps the answer as a source,
observed by `browser`. See PLAN M19, third cut.

Rendering. For a fetch with `render`, the extension opens the page in a tab of this
browser instead. It does not show this tab. When the page stops changing, the extension
reads the page as its scripts built it. The same rules hold the navigation of the tab, in
the top frame and in its frames, to the same hosts. The extension closes the tab after it
reads it. The page itself loads its scripts and images. These loads belong to the page,
as they do when you open the page.

The permissions, and why the extension needs them:

- `storage` and `alarms` keep the pairing, and connect the socket again.
- `scripting` reaches an amem page that is already open, and reads a rendered tab.
- `tabs` opens and watches that tab.
- `declarativeNetRequest` holds requests to the hosts.
- `webRequest` reads where a stopped request was going. It never blocks a request.
- The host permissions on http and https let a fetch carry your cookies to a host that an
  ask names.
