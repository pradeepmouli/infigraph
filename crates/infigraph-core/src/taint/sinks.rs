pub struct TaintSink {
    pub kind: &'static str,
    pub category: &'static str,
    pub patterns: &'static [&'static str],
    pub extensions: Option<&'static [&'static str]>,
}

pub static TAINT_SINKS: &[TaintSink] = &[
    // SQL execution
    TaintSink {
        kind: "SqlQuery",
        category: "SqlInjection",
        patterns: &[
            "execute(",
            "cursor.execute(",
            "executemany(",
            "raw_query(",
            "rawQuery(",
            "executeQuery(",
            "executeUpdate(",
            "connection.query(",
            "db.Query(",
            "db.Exec(",
            "db.QueryRow(",
            "SqlCommand(",
            "ExecuteReader(",
            "ExecuteNonQuery(",
            "query!(",
            "sqlx::query(",
        ],
        extensions: None,
    },
    // Command execution
    TaintSink {
        kind: "CommandExec",
        category: "CommandInjection",
        patterns: &[
            "os.system(",
            "os.popen(",
            "subprocess.call(",
            "subprocess.run(",
            "subprocess.Popen(",
            "subprocess.check_output(",
            "exec(",
            "child_process.exec(",
            "child_process.execSync(",
            "Runtime.getRuntime().exec(",
            "ProcessBuilder(",
            "Process.Start(",
            "cmd.Run(",
            "cmd.Output(",
            "std::process::Command::new(",
        ],
        extensions: None,
    },
    // HTML rendering (XSS sinks)
    TaintSink {
        kind: "HtmlRender",
        category: "XssRisk",
        patterns: &[
            "innerHTML",
            "outerHTML",
            "dangerouslySetInnerHTML",
            "document.write(",
            "document.writeln(",
            "mark_safe(",
            "|safe",
            "Markup(",
            "Html.Raw(",
            "template.HTML(",
        ],
        extensions: None,
    },
    // File system access
    TaintSink {
        kind: "FileAccess",
        category: "PathTraversal",
        patterns: &[
            "open(",
            "os.path.join(",
            "Path.join(",
            "readFile(",
            "writeFile(",
            "fs.readFile(",
            "fs.writeFile(",
            "Files.write(",
            "Files.read(",
            "File.WriteAllText(",
            "File.ReadAllText(",
            "os.Open(",
            "os.Create(",
            "os.WriteFile(",
            "std::fs::read(",
            "std::fs::write(",
        ],
        extensions: None,
    },
    // Redirect
    TaintSink {
        kind: "Redirect",
        category: "OpenRedirect",
        patterns: &[
            "redirect(",
            "res.redirect(",
            "response.redirect(",
            "location.href",
            "window.location",
            "HttpResponseRedirect(",
            "sendRedirect(",
            "Response.Redirect(",
            "http.Redirect(",
        ],
        extensions: None,
    },
    // Deserialization
    TaintSink {
        kind: "Deserialize",
        category: "InsecureDeserialization",
        patterns: &[
            "pickle.loads(",
            "pickle.load(",
            "yaml.load(",
            "yaml.unsafe_load(",
            "unserialize(",
            "JSON.parse(",
            "ObjectInputStream(",
            "readObject(",
            "BinaryFormatter.Deserialize(",
            "json.Unmarshal(",
        ],
        extensions: None,
    },
    // LDAP injection
    TaintSink {
        kind: "LdapQuery",
        category: "LdapInjection",
        patterns: &[
            "ldap.search(",
            "ldap_search(",
            "search_s(",
            "DirectorySearcher(",
            "SearchRequest(",
        ],
        extensions: None,
    },
    // XPath injection
    TaintSink {
        kind: "XPathQuery",
        category: "XPathInjection",
        patterns: &[
            "xpath(",
            "evaluate(",
            "selectNodes(",
            "XPathExpression(",
            "XPathNavigator.Select(",
        ],
        extensions: None,
    },
];

pub static TAINT_SANITIZERS: &[TaintSanitizer] = &[
    TaintSanitizer {
        category: "SqlInjection",
        patterns: &[
            "parameterize",
            "prepare(",
            "bind_param",
            "sanitize_sql",
            "placeholder",
            "?)",
            "%s)",
            "prepared_statement",
        ],
    },
    TaintSanitizer {
        category: "XssRisk",
        patterns: &[
            "escape_html",
            "html.escape(",
            "cgi.escape(",
            "sanitize(",
            "DOMPurify",
            "bleach.clean(",
            "encodeURIComponent(",
            "markupsafe.escape(",
            "HtmlEncoder.Encode(",
        ],
    },
    TaintSanitizer {
        category: "CommandInjection",
        patterns: &[
            "shlex.quote(",
            "shell_escape",
            "escapeshellarg(",
            "shell=False",
            "shlex.split(",
        ],
    },
    TaintSanitizer {
        category: "PathTraversal",
        patterns: &[
            "realpath(",
            "abspath(",
            "canonicalize(",
            "path.resolve(",
            "secure_filename(",
            "os.path.basename(",
            "filepath.Clean(",
        ],
    },
    TaintSanitizer {
        category: "OpenRedirect",
        patterns: &[
            "url_has_allowed_host(",
            "is_safe_url(",
            "validate_redirect(",
            "safe_redirect(",
        ],
    },
    TaintSanitizer {
        category: "InsecureDeserialization",
        patterns: &["safe_load(", "yaml.safe_load(", "SafeLoader", "allowlist"],
    },
    TaintSanitizer {
        category: "LdapInjection",
        patterns: &["ldap.filter.escape(", "escape_filter_chars("],
    },
    TaintSanitizer {
        category: "XPathInjection",
        patterns: &["xpath_escape(", "parameterized_xpath("],
    },
];

pub struct TaintSanitizer {
    pub category: &'static str,
    pub patterns: &'static [&'static str],
}
