header! {
    #[doc="`Server` header, defined in [RFC7231](http://tools.ietf.org/html/rfc7231#section-7.4.2)"]
    #[doc=""]
    #[doc="The `Server` header field contains information about the software"]
    #[doc="used by the origin server to handle the request, which is often used"]
    #[doc="by clients to help identify the scope of reported interoperability"]
    #[doc="problems, to work around or tailor requests to avoid particular"]
    #[doc="server limitations, and for analytics regarding server or operating"]
    #[doc="system use.  An origin server MAY generate a Server field in its"]
    #[doc="responses."]
    #[doc=""]
    #[doc="# ABNF"]
    #[doc="```plain"]
    #[doc="Server = product *( RWS ( product / comment ) )"]
    #[doc="```"]
    // TODO: Maybe parse as defined in the spec?
    (Server, "Server") => [String]
}

bench_header!(bench, Server, { vec![b"Some String".to_vec()] });
