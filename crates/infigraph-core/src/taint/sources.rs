pub struct TaintSource {
    pub kind: &'static str,
    pub patterns: &'static [&'static str],
    pub extensions: Option<&'static [&'static str]>,
}

pub static TAINT_SOURCES: &[TaintSource] = &[
    // HTTP parameters
    TaintSource {
        kind: "HttpParam",
        patterns: &[
            "request.GET[", "request.GET.get(", "request.POST[", "request.POST.get(",
            "request.args.get(", "request.args[", "request.form[", "request.form.get(",
            "req.query.", "req.query[", "req.params.", "req.params[",
            "request.getParameter(", "request.getParameterValues(",
            "@RequestParam", "@PathVariable", "@QueryParam",
            "Request.Query[", "Request.Form[",
            "c.Param(", "c.Query(", "c.DefaultQuery(",
            "r.URL.Query()", "r.FormValue(",
        ],
        extensions: None,
    },
    // HTTP body
    TaintSource {
        kind: "HttpBody",
        patterns: &[
            "request.body", "req.body", "request.json", "request.data",
            "request.get_json(", "request.content",
            "@RequestBody", "request.getInputStream(",
            "Request.Body", "ReadFromJsonAsync(",
            "c.BindJSON(", "c.ShouldBindJSON(",
            "json.NewDecoder(r.Body)",
        ],
        extensions: None,
    },
    // HTTP headers
    TaintSource {
        kind: "HttpHeader",
        patterns: &[
            "request.headers[", "request.headers.get(",
            "req.headers[", "req.headers.get(", "req.header(",
            "request.getHeader(", "request.META[",
            "Request.Headers[",
            "r.Header.Get(",
        ],
        extensions: None,
    },
    // File reads
    TaintSource {
        kind: "FileRead",
        patterns: &[
            "open(", "readFile(", "fs.read", "File(",
            "fs.readFileSync(", "fs.readFile(",
            "Files.readAllBytes(", "Files.readString(",
            "File.ReadAllText(", "File.ReadAllLines(",
            "os.ReadFile(", "ioutil.ReadFile(",
        ],
        extensions: None,
    },
    // User input (CLI/console)
    TaintSource {
        kind: "UserInput",
        patterns: &[
            "input(", "readline(", "Scanner(",
            "process.stdin", "sys.stdin",
            "Console.ReadLine(", "bufio.NewReader(os.Stdin)",
            "std::io::stdin()",
        ],
        extensions: None,
    },
    // Environment variables (can be attacker-controlled in some contexts)
    TaintSource {
        kind: "EnvVar",
        patterns: &[
            "os.environ[", "os.environ.get(", "os.getenv(",
            "process.env.", "process.env[",
            "System.getenv(", "Environment.GetEnvironmentVariable(",
            "os.Getenv(",
            "std::env::var(",
        ],
        extensions: None,
    },
];
