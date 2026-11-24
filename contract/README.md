# HTTP contract

The public contract of the corx link-preview service, for client authors.
`preview.schema.json` is the machine-readable form of every response body;
`vectors/` is the executable form — each JSON file scripts a request, the
upstream hops it may trigger, and the exact expected response. The
`crates/corx` test suite validates bodies against the schema and replays
every vector through the pipeline, so this document and the code cannot
drift apart silently.

## Endpoint

`GET|HEAD /v1/preview?url=<percent-encoded URL>`.

`OPTIONS /v1/preview` answers a CORS preflight. Any other method gets
`405 method_not_allowed` with `Allow: GET, HEAD, OPTIONS`; any other path
gets `404 not_found`. HEAD runs the identical pipeline — adapters strip the
body.

The `url` parameter is required, percent-encoded, and at most 2048 bytes
after decoding and normalization (percent-encoding can expand it); the
first occurrence wins when it is repeated. It must be
an `http` or `https` URL with a dotted public domain — no credentials, no
explicit port, no IP literal, and not `localhost` or a name ending in
`.localhost`, `.local`, `.internal`, `.arpa`, or `.onion`.

## CORS

With an allowlist configured (`ALLOWED_ORIGINS` as a comma-separated
origin list), every response carries `Vary: Origin`; a request whose
`Origin` is on the list additionally carries
`Access-Control-Allow-Origin` echoing it. A request whose `Origin` is not
on the list gets `403 forbidden_origin` (and no `Access-Control-Allow-Origin`).
A request with no `Origin` header is always allowed — CORS only constrains
browsers. With `ALLOWED_ORIGINS=*` every response carries
`Access-Control-Allow-Origin: *` and no `Vary`.

Preflight (`OPTIONS` with an allowed origin) answers `204` with
`Access-Control-Allow-Methods: GET, HEAD, OPTIONS` and
`Access-Control-Max-Age: 86400`.

## Success bodies

`200` responses carry `content-type: application/json; charset=utf-8` and a
`kind` discriminator with two possible values.

### `kind: "page"`

An HTML page's metadata:

| field | type | rule |
|---|---|---|
| `url` | string URI | the request target, normalized |
| `finalUrl` | string URI | the last hop after redirects |
| `title` | string, ≤ 300 chars | `og:title` → `twitter:title` → `<title>` |
| `description` | string, ≤ 600 chars | `og:description` → `twitter:description` → `description` |
| `siteName` | string, ≤ 100 chars | `og:site_name` |
| `image` | object | first resolvable of `og:image:secure_url`, `og:image:url`, `og:image`, `twitter:image`, `twitter:image:src` |
| `image.url` | string URI | resolved against `<base href>` when it is http(s) |
| `image.width`/`image.height` | integer > 0 | `og:image:width`/`height`; only with an `og:image*` winner; zero and malformed values dropped |
| `image.alt` | string, ≤ 300 chars | `og:image:alt` → `twitter:image:alt` |
| `favicon` | string URI | first `rel=icon`, else first `rel=apple-touch-icon*` |
| `card` | string, ≤ 50 chars | `twitter:card` |
| `type` | string, ≤ 50 chars | `og:type` |
| `themeColor` | string, ≤ 32 chars | `theme-color` |
| `lang` | string, ≤ 35 chars | `html[lang]` |
| `source` | `"html"` \| `"oembed"` | where the metadata came from |
| `fetchedAt` | integer ≥ 0 | Unix seconds when the final hop was fetched |

All optional fields are absent (never `null`) when the page does not
provide them. Text fields are entity-decoded, whitespace-collapsed, and
truncated at their caps; URL fields are entity-decoded but never
truncated — an over-long or unresolvable URL is dropped instead. A page
must yield a `title` or an `image`; anything less is
`404 no_metadata`.

`source` is `"html"` when the metadata was extracted from the target's
own HTML head. `"oembed"` marks a preview resolved through a provider's
oEmbed endpoint: when the final HTML hop's host has a registered oEmbed
provider — currently `reddit.com` and `tiktok.com`, including their
subdomains — and the HTML outcome is an `upstream_status` error, a
`no_metadata` error, or a page with neither `description` nor `image`,
the service makes exactly one `GET` to the provider's oEmbed endpoint
(`<endpoint>?url=<finalUrl>&format=json`). That request sends
`Accept: application/json`, follows no redirects, reads at most 64 KiB,
and must return a 2xx JSON body — any other outcome keeps the original
result unchanged. Of the oEmbed fields, `title` → `title`,
`author_name` → `description`, `provider_name` → `siteName`, and
`thumbnail_url` (resolved against the endpoint URL) + `thumbnail_width` /
`thumbnail_height` (numbers or numeric strings) → `image`; the `html`
field is never read, and an oEmbed page that would yield neither `title`
nor `image` is discarded. The exchange runs inside the same 6 s
budget as the HTML pipeline.

### `kind: "media"`

A non-HTML resource described by its response headers — the body is never
read:

| field | type | rule |
|---|---|---|
| `url`, `finalUrl`, `fetchedAt` | as above | |
| `mediaType` | string `type/subtype` | the `Content-Type` essence, lowercased |
| `size` | integer ≥ 0 | `Content-Length`, present only when the body is unencoded (`Content-Encoding` absent or `identity`) — an encoded body's length would describe the compressed form |

## Error bodies

Every non-success response (except the `204` preflight, which is empty) is
`{"error": <slug>, "message": <sentence>}`. `message` is stable but meant
for humans; `error` is the machine contract. An `upstream_status` body
additionally carries `upstreamStatus`: the final hop's status code as an
integer — any non-2xx status that is not one of the five followed
redirects, so 300/304, 4xx and 5xx all land here. `upstreamStatus` never
appears on any other error.

| status | `error` | meaning | cacheable | extra headers |
|---|---|---|---|---|
| 400 | `invalid_url` | `url` missing, malformed, or disallowed syntax | no | |
| 403 | `forbidden_origin` | `Origin` not on the allowlist | no | |
| 403 | `blocked_target` | the target or a redirect hop points at a disallowed host | no | |
| 404 | `not_found` | unknown path | no | |
| 404 | `no_metadata` | the page has neither title nor image | yes (`max-age=3600`) | |
| 405 | `method_not_allowed` | method other than GET/HEAD/OPTIONS | no | `Allow` |
| 413 | `too_large` | HTML exceeded 1 MiB before `</head>` | no | |
| 415 | `unsupported_content` | `Content-Type` missing or unparseable | no | |
| 422 | `redirect_failed` | a followed 3xx had a missing, non-ASCII or unparseable `Location`, a `Location` resolving to a non-http(s) URL, credentials, an explicit port or more than 2048 bytes after normalization, or the chain exceeded 5 hops | yes (`max-age=3600`) | |
| 422 | `upstream_status` | the final hop answered a non-2xx, non-followed status | only a 4xx other than 408/429 (`max-age=3600`) | `upstreamStatus` in the body |
| 429 | `rate_limited` | client or per-host limit exceeded | no | `Retry-After` (seconds, ≥ 1) |
| 502 | `upstream_unreachable` | the target could not be reached | no | |
| 503 | `overloaded` | the deployment is shedding load | no | |
| 504 | `upstream_timeout` | an upstream hop or the total budget expired | no | |

Error responses also carry `x-content-type-options: nosniff`. Successful
responses carry it too.

## `Cache-Control`

- success: `public, max-age=86400, stale-while-revalidate=604800`
- `no_metadata` and `redirect_failed`: `public, max-age=3600`
- `upstream_status`: `public, max-age=3600` when the upstream status is
  a 4xx other than 408 or 429; `no-store` otherwise
- every other error: `no-store`

## Limits and upstream behaviour

- Normalized target URL ≤ 2048 bytes; at most 5 redirects; ≤ 1 MiB of HTML is read.
- Upstream connect deadline 3 s; total preview budget 6 s.
- Upstream requests send a single deployment-configured `User-Agent` and
  never cookies. `Accept` is `text/html,application/xhtml+xml` on HTML
  hops and `application/json` on the oEmbed exchange. Redirects are
  followed by the service itself, not the HTTP stack, so every hop is
  re-validated against the target rules — an `https` → `http` downgrade
  or a hop to a private/literal address is refused.

## Versioning

The path is `/v1`; this document is contract v1, introduced with corx
0.4. Breaking changes (a renamed slug, a dropped field, a changed status)
ship under a new path version.
