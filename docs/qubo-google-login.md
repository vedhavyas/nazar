# Qubo Google (GM) login: the client ids, the login call, and the durable-login experiment

Extracted offline 2026-10-15 from the app's jadx decompile, base_x/resources.arsc (byte-identical
in base.apk), the native libs and the existing Ghidra dumps. No live call was made while writing
this. The experiment below needs exactly ONE browser consent from Ved; every other step runs
mechanically from this machine.

The question it settles: the phone-OTP route gives 1h sessions with no refresh token (spec 3.3),
while the Google route returns a refreshToken (verified live 2026-10-14, spec:587). This experiment
mints the Google ID token OUTSIDE the app (browser only) and feeds it to the same login call. If the
backend accepts it, durable login no longer needs the phone app at all. If it rejects it with an
audience error, we learn the backend pins the token's `aud` to the app's web client id.

## 1. Client ids (the only three `*.apps.googleusercontent.com` strings in the app)

| where | value | role |
|---|---|---|
| `web_server_client_id` (string res 0x7f130a30) | `398700023943-i6bmv7jos4tnulk9p1pjdcbrjongverk.apps.googleusercontent.com` | the serverClientId the app passes to `requestIdToken` - this is the ID token's `aud` |
| `client_id` (string res 0x7f1300ca) | `398700023943-iev6bdmqd54e6mgee3g4aeri35odbg01.apps.googleusercontent.com` | Android client, same GCP project 398700023943 |
| `default_web_client_id` (string res 0x7f130116) | `5553011333-l9hblkht19sg63kn6t86ot33is230sc4.apps.googleusercontent.com` | Firebase auto-resource (project 5553011333, `google_app_id` = `1:5553011333:android:e900000000000001`). NOT used by the sign-in path |

Sources: values resolved from `base_x/resources.arsc` by resource id; ids from jadx
`R.java:17097` (web_server_client_id), `R.java:14691` (client_id), `R.java:14767`
(default_web_client_id); the same three strings are in `base.apk`'s arsc. The native libs carry
NO client id (grep across all `.so` in `arm64_x/lib/arm64-v8a/` finds only the two log strings
`socialLogin : getProfile for gmail login id...` / `saveUserInDB gmail login id...` in
libcore-main). Beware a `strings` artifact: the raw arsc output shows "HH"/"FF" byte prefixes -
those are UTF-8 length-prefix bytes (0x48/0x46), not part of the ids.

## 2. How the app mints the ID token

`GetStartedFragment.m8()` (jadx `com/hero/iot/ui/landing/getstarted/GetStartedFragment.java:252-256`):

```java
new GoogleSignInOptions.Builder(GoogleSignInOptions.DEFAULT_SIGN_IN)  // scopes: openid, profile (GoogleSignInOptions.java:91-94, 324-326)
    .requestEmail()                    // adds Scope("email")   (GoogleSignInOptions.java:330-333)
    .requestIdToken(web_server_client_id)        // (.d(), GoogleSignInOptions.java:340-345)
    .requestServerAuthCode(web_server_client_id) // (.g(), GoogleSignInOptions.java:358-361)
    .build();
GoogleSignIn.getClient(activity, options).getSignInIntent();   // Play Services account picker
```

On the result: `account.getIdToken()`, logged verbatim as `Token ID : ...`
(`GetStartedFragment.java:212`, `LoginActivity.java:256`), then handed to native
`N0("GM", idToken, "RELEASE", dto)` (`GetStartedFragment.java:225`, `LoginActivity.java:269`) ->
`Mobile_userManager::socialLogin` (libcore-mobile.so 0x158164). Same builder chain in
`LoginActivity.java:272-276` and, for silent re-auth when `KEY_LOGIN_TYPE == "GM"`,
`HeroApplicationApp.java:1079-1085`.

So the token to replicate: scopes `openid email profile`, serverClientId = the
`web_server_client_id` above, and its `aud` should equal that same id. Confirmable offline: decode
the `aud` claim of any app-minted token from logcat (step 0).

## 3. The login call (the exchange)

From the native path (PROTOCOL.md 7.1-7.3; implemented in `qubo-spike/p2p/sso_login.py`):

```
POST https://srvcapp.platform.quboworld.com:443/sms/api/v4/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/login?system=GM&type=RELEASE
Content-Type: application/json
App-Id: 934488E68332E88B1E0F9AF552840184955629777525A195949C0BE97DEF6455
Source: ANDROID

{"accessToken": "<Google ID token>",
 "username": "",
 "password": "",
 "deviceAttribute": "<MANUFACTURER>|<MODEL>|<PRODUCT>"}
```

- No Authorization header (native uses `createConnectionWithoutAuth`).
- App-Id constant: jadx `U5/a.java:57`; host `U5/a.java:54`. spid is the same constant used
  everywhere.
- Response keys parsed from the body: `uuid`, `accessToken`, `refreshToken`, `newUser`, `name`
  (PROTOCOL.md 7.2). A refreshToken IS present on this route (live-verified 2026-10-14: spec:587,
  findings:85-98).
- `deviceAttribute` is client-built and not validated server-side (findings soft spot 2);
  sso_login.py sends `Darwin|arm64|qubopy`.
- Token TTLs are NOT extracted. Read them from the response JWTs' `exp` claims during the
  experiment. The durable part is the refresh route (spec 3.2):
  `POST /sms/api/v1/sp/{sp}/users/{uuid}/auth/refresh` with `{accessToken, refreshToken}`.

## 4. The experiment

### Step 0 - pre-flight: pin the expected audience (optional; needs one app sign-in by Ved)

On the phone, Google sign-in once; from this machine:

```
adb logcat | grep -iE "Token ID|GetStartedFragment"
```

then decode the copied token locally (no signature check, anything with python):

```python
import base64, json, sys
t = sys.argv[1].split(".")[1]; t += "=" * (-len(t) % 4)
c = json.loads(base64.urlsafe_b64decode(t))
print({k: c.get(k) for k in ("aud", "iss", "exp", "email")})
```

`aud` is the exact audience the backend is shown by the app. If `aud` != web_server_client_id
(section 1), Google has changed semantics and step 1a's assumption is wrong - report it.

### Step 1a - browser attempt with THAT client id [VED: one browser consent, may fail before consent]

**RESULT (2026-10-16, run mechanically):** `redirect_uri_mismatch` - the page landed on `accounts.google.com/signin/oauth/error?authError=...` whose base64 decodes to `redirect_uri_mismatch`, exactly as predicted. No consent spent; the web-client id cannot be driven from this machine without GCP console access (Hero's project - a gate, not a step).

Open this URL in any browser (being signed into Ved's Google account, the one the Qubo app uses):

```
https://accounts.google.com/o/oauth2/v2/auth?client_id=398700023943-i6bmv7jos4tnulk9p1pjdcbrjongverk.apps.googleusercontent.com&redirect_uri=http%3A%2F%2Flocalhost%3A8765%2F&response_type=id_token&scope=openid%20email%20profile&nonce=qubo1&prompt=select_account
```

What this client id can/cannot do: it is a Web-application client (Google requires serverClientId
to be a web client; its resource name says so). Web clients accept only REGISTERED redirect URIs,
so `http://localhost:8765/` is almost certainly not registered and Google will stop with
`Error 400: redirect_uri_mismatch` BEFORE any consent. That error is a fine result - record it and
move to 1b, no consent spent. Do NOT attempt a code-exchange with this client even if it consents:
a web client's token endpoint requires its client secret, which is not in the APK; the id_token
implicit shape above needs no secret. If by some chance consent succeeds, the browser lands on
`http://localhost:8765/#id_token=...` (connection refused is fine - copy the FULL address-bar URL);
that token has exactly the right `aud` and would be the best possible outcome.

Note on "making it a desktop client": you cannot convert this id. Desktop-type clients are separate
registrations with their own client ids, so their tokens carry a DIFFERENT `aud`. The only clean way
to mint `aud = web_server_client_id` from a desktop is to register a localhost redirect on THIS web
client, which needs GCP console access to project 398700023943 (Hero's project; flag as a gate, not
a step in this recipe).

### Step 1b - browser attempt with the Android client id [VED: one browser consent]

**RESULT (2026-10-16, run with Ved's sign-in):** both browser flows refused by Google BEFORE any consent. `response_type=id_token` landed on `signin/oauth/error` with `invalid_request` ("Access blocked: Qubo's request is invalid"); the doc's code + PKCE fallback (`response_type=code`, S256 challenge, the same custom-scheme redirect) landed on the same `invalid_request` for the same client. The Android client cannot be driven from a plain browser either, and the web client cannot without registering a redirect in Hero's GCP project (a gate). **Conclusion: a headless browser mint does not exist.** The durable path is the one-time app-minted token (the Step 0 logcat control run - the genuine app is the only thing that can mint this client's token, which is presumably the point): its exchange returns the refreshToken, and the refresh route renews forever after. The login screen's Google button therefore has no mint to call in v1; the exchange route (`POST /api/login/google`) stays, fed once with an app-minted token.

**Attempts against the app itself (2026-10-16, all walled):** the app's logcat never prints its ID token in this build (three sign-ins incl. a manual one; a full-buffer JWT scan is empty) and it pins its TLS (every PCAPdroid capture, third-party and manual, carries only ciphertext even with the MITM addon; the same holds for the chase's own 14-Oct captures), no root on the phone, `allowBackup=false` blocks adb backup, and its external storage holds only cached stills. A Play-Store emulator (API 30, arm64) runs the app fine but Play images are `user` builds (no adb root, measured), and the read-from-the-AVD-userdata route was abandoned by Ved's call before the account sign-in - the emulator UX made the sign-in impractical (clipboard dead, recovery challenge expiring, no cross-device flow on the 2020-era sign-in stack). The durable token therefore remains pending a capture route; the OTP re-test (below) is the live thread.

Loopback is NOT allowed for Android-type clients - Google's auto-registered redirect for an Android
client is the custom scheme `com.googleusercontent.apps.<client-id-without-suffix>:/oauth2redirect`.
(For a PKCE loopback you would need a Desktop client id, which we do not have.) Open:

```
https://accounts.google.com/o/oauth2/v2/auth?client_id=398700023943-iev6bdmqd54e6mgee3g4aeri35odbg01.apps.googleusercontent.com&redirect_uri=com.googleusercontent.apps.398700023943-iev6bdmqd54e6mgee3g4aeri35odbg01%3A%2Foauth2redirect&response_type=id_token&scope=openid%20email%20profile&nonce=qubo1&prompt=select_account
```

Payoff here is narrower than 1a: the resulting ID token's `aud` is the ANDROID client id
(398700023943-iev6bd...), not the web_server_client_id. Whether the backend accepts it is exactly
what this experiment decides (same project, different client).

- Method: complete the Google consent for the correct Google account. The browser then tries to
  open the `com.googleusercontent.apps...` scheme (usually an error page or an "Open app?" dialog -
  cancel it). The token is in the address-bar URL after the `#`: copy the WHOLE URL.
- If the browser strips the fragment or Google refuses `response_type=id_token` for this client
  type, use code + PKCE instead: same URL but
  `response_type=code&code_challenge=<base64url(sha256(verifier))>&code_challenge_method=S256`
  (verifier = 43-128 random chars), read `?code=...` from the landing URL, then exchange
  mechanically (no client secret - Android clients are public clients):

```
curl -sS https://oauth2.googleapis.com/token \
  -d 'grant_type=authorization_code' \
  -d 'code=<code>' \
  -d 'client_id=398700023943-iev6bdmqd54e6mgee3g4aeri35odbg01.apps.googleusercontent.com' \
  -d 'redirect_uri=com.googleusercontent.apps.398700023943-iev6bdmqd54e6mgee3g4aeri35odbg01:/oauth2redirect' \
  -d 'code_verifier=<verifier>'
```

### Step 2 - the login call [mechanical]

```
python /Users/vedhavyas/Projects/qubo-spike/p2p/sso_login.py --id-token "<id token from 1a/1b>"
# or --id-token-file /tmp/idtoken.txt ; prints HTTP status and the raw body (truncated to 800 chars)
```

Raw equivalent (if the script is unavailable):

```
curl -sS 'https://srvcapp.platform.quboworld.com:443/sms/api/v4/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/login?system=GM&type=RELEASE' \
  -H 'Content-Type: application/json' \
  -H 'App-Id: 934488E68332E88B1E0F9AF552840184955629777525A195949C0BE97DEF6455' \
  -H 'Source: ANDROID' \
  -d '{"accessToken":"<ID TOKEN>","username":"","password":"","deviceAttribute":"Darwin|arm64|qubopy"}'
```

### Step 3 - success and failure criteria

| outcome | meaning | action |
|---|---|---|
| 200 + non-empty `refreshToken`, `newUser` false, `uuid` matches the account uuid in `~/.qubopy_session.json` | durable login SOLVED headlessly | decode `exp` of both returned tokens; exercise the refresh route (spec 3.2) and confirm a fresh accessToken comes back; then `--save` |
| 200 but `newUser: true` or an unexpected `uuid` | the consent used a Google account with no Qubo account - a login here would CREATE a new cloud user | do NOT save; redo 1b consent with the correct Google account |
| 401/403, body naming an audience/token problem (e.g. `ERR_AUTH_002`, or text mentioning `aud`/client id) | the backend pins `aud` to the app's web client id; the Android-client token is rejected | record the EXACT body. Login stays on the phone-logcat token for now (below); durable headless mint needs the GCP console fix from 1a - a gate |
| anything else | unknown | record HTTP status + raw body, adjust, rerun |

Fallbacks and notes:

- Known-good token source (no OAuth plumbing): the app prints its own ID token as
  `Token ID : ...` in logcat (step 0); feeding THAT to the step-2 command is the route already
  live-verified 2026-10-14. It costs one app sign-in and still yields the refreshToken - use it
  as the control run if 1b's outcome is ambiguous.
- Endpoint version: v4 per PROTOCOL.md 7.1, live-verified. If it ever 404s, the sibling family is
  `/sms/api/v1/sp/<sp>/user/login?system=...` (the v1 template exists in libcore-mobile; the email
  login in the spec uses v1). Record the error before switching.
- Not worth trying: `default_web_client_id` (different Firebase project, unused by the sign-in
  path); `urn:ietf:wg:oauth:2.0:oob` (Google disabled OOB flows); Google device-code flow (only
  for TV/Limited-input client types - this client id is not one).
- Consent hygiene: the consent must be for the Google account that owns the Qubo login, otherwise
  a `newUser: true` response can create a stray cloud user. Everything in this recipe except the
  browser consents is local except the one POST (the app's own login call).
