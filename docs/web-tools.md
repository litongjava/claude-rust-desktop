# Bounded web tools

`WebFetch` returns readable text and HTTP(S) links from the page's main content. Scripts, styles, navigation, forms, hidden elements, and other page chrome are omitted. This avoids feeding hundreds of thousands of HTML tokens back into the next model request.

- Default output: 12,000 Unicode characters; `max_chars` may request up to 50,000.
- `offset` and the returned `next_offset` allow subsequent pages of extracted text.
- `raw: true` requests original markup while preserving output limits.
- Downloads stop at 2 MiB and have a 12-second request deadline (4-second connection timeout).
- Results report the final URL, format, downloaded bytes, extracted and returned character counts, truncation, and continuation offset. A truncated source cannot be completely paged; use a more specific endpoint.
- Text, HTML, JSON, and XML are supported. HTTP failures and unsupported binary formats fail the tool instead of appearing as successful content.

`WebSearch` queries DuckDuckGo's HTML results and Bing's RSS results concurrently, returning the first usable provider with up to eight titles, URLs, and bounded snippets. Requests have a five-second deadline. If every provider fails, returns no usable results, or shows a bot challenge, the tool reports that search is unavailable. This replaces a sequential 30-second Searx attempt and an instant-answer API that did not provide general web search results. Public providers may still rate-limit or block requests.

Run deterministic tests with `cargo test --lib tools::web::tests`. An optional live smoke test is available with `cargo test --lib tools::web::tests::live_public_search_and_changelog -- --ignored --nocapture`; it reports search availability and changelog response size without printing page bodies.
