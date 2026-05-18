use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::graph::GraphQuery;

/// A detected HTTP route/endpoint in the codebase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// HTTP method (GET, POST, PUT, DELETE, PATCH, or UNKNOWN)
    pub method: String,
    /// Inferred URL path (best-effort from symbol/docstring heuristics)
    pub path: String,
    /// Symbol ID of the handler function
    pub handler_id: String,
    /// File containing the handler
    pub file: String,
    /// Detected web framework (e.g. "flask", "express", "spring", "actix")
    pub framework: String,
}

/// Detect HTTP routes/endpoints from the indexed code graph using heuristics.
///
/// Queries symbols and applies language-aware pattern matching on names and
/// docstrings to identify likely HTTP handlers. This is intentionally broad
/// to catch routes across many web frameworks.
pub fn detect_routes(gq: &GraphQuery) -> Result<Vec<Route>> {
    let rows = gq.raw_query(
        "MATCH (s:Symbol) WHERE s.kind IN ['Function', 'Method'] \
         RETURN s.id, s.name, s.kind, s.file, s.docstring",
    )?;

    let mut routes = Vec::new();

    for row in &rows {
        let id = &row[0];
        let name = &row[1];
        let _kind = &row[2];
        let file = &row[3];
        let docstring = row.get(4).map(|s| s.as_str()).unwrap_or("");

        if let Some(route) = detect_route_from_symbol(id, name, file, docstring) {
            routes.push(route);
        }
    }

    // Sort by file, then path for stable output
    routes.sort_by(|a, b| a.file.cmp(&b.file).then(a.path.cmp(&b.path)));

    Ok(routes)
}

/// Try to detect a route from a single symbol's name and docstring.
fn detect_route_from_symbol(id: &str, name: &str, file: &str, docstring: &str) -> Option<Route> {
    let name_lower = name.to_lowercase();
    let doc_lower = docstring.to_lowercase();

    // Determine language from file extension
    let lang = language_from_file(file);

    // Try docstring-based detection first (strongest signal — often contains
    // explicit route/endpoint annotations captured as docstrings)
    if let Some(route) = detect_from_docstring(id, name, file, &doc_lower) {
        return Some(route);
    }

    // Then try name-based heuristics per language
    match lang {
        Lang::Python => detect_python_route(id, name, &name_lower, file, &doc_lower),
        Lang::JavaScript | Lang::TypeScript => {
            detect_js_ts_route(id, name, &name_lower, file, &doc_lower)
        }
        Lang::Go => detect_go_route(id, name, &name_lower, file, &doc_lower),
        Lang::Java => detect_java_route(id, name, &name_lower, file, &doc_lower),
        Lang::Rust => detect_rust_route(id, name, &name_lower, file, &doc_lower),
        Lang::Ruby => detect_ruby_route(id, name, &name_lower, file, &doc_lower),
        Lang::Php => detect_php_route(id, name, &name_lower, file, &doc_lower),
        Lang::CSharp => detect_csharp_route(id, name, &name_lower, file, &doc_lower),
        Lang::Elixir => detect_elixir_route(id, name, &name_lower, file, &doc_lower),
        Lang::Other => detect_generic_route(id, name, &name_lower, file, &doc_lower),
    }
}

// ---------------------------------------------------------------------------
// Docstring-based detection (language-agnostic)
// ---------------------------------------------------------------------------

fn detect_from_docstring(id: &str, name: &str, file: &str, doc_lower: &str) -> Option<Route> {
    // Look for explicit HTTP method keywords in docstrings
    let http_methods = [
        ("get ", "GET"),
        ("post ", "POST"),
        ("put ", "PUT"),
        ("delete ", "DELETE"),
        ("patch ", "PATCH"),
    ];

    // Pattern: docstring mentions route/endpoint/api along with an HTTP method
    let has_route_context = doc_lower.contains("route")
        || doc_lower.contains("endpoint")
        || doc_lower.contains("api")
        || doc_lower.contains("handler")
        || doc_lower.contains("@app.")
        || doc_lower.contains("@router.")
        || doc_lower.contains("handlefunc")
        || doc_lower.contains("mapping");

    if !has_route_context {
        return None;
    }

    // Try to extract method from docstring
    let method = http_methods
        .iter()
        .find(|(kw, _)| doc_lower.contains(kw))
        .map(|(_, m)| m.to_string())
        .unwrap_or_else(|| "GET".to_string());

    // Try to extract a path from the docstring (look for /something patterns)
    let path = extract_path_from_text(doc_lower)
        .unwrap_or_else(|| format!("/{}", name.to_lowercase()));

    Some(Route {
        method,
        path,
        handler_id: id.to_string(),
        file: file.to_string(),
        framework: detect_framework_from_docstring(doc_lower),
    })
}

// ---------------------------------------------------------------------------
// Python: Flask, FastAPI, Django, Starlette
// ---------------------------------------------------------------------------

fn detect_python_route(
    id: &str,
    _name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    // Python naming patterns for HTTP handlers
    let method_prefixes = [
        ("get_", "GET"),
        ("post_", "POST"),
        ("put_", "PUT"),
        ("delete_", "DELETE"),
        ("patch_", "PATCH"),
        ("handle_", "UNKNOWN"),
        ("on_get", "GET"),
        ("on_post", "POST"),
        ("on_put", "PUT"),
        ("on_delete", "DELETE"),
    ];

    for (prefix, method) in &method_prefixes {
        if name_lower.starts_with(prefix) {
            let path_part = &name_lower[prefix.len()..];
            let path = if path_part.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", path_part.replace('_', "/"))
            };
            return Some(Route {
                method: method.to_string(),
                path,
                handler_id: id.to_string(),
                file: file.to_string(),
                framework: detect_python_framework(doc_lower),
            });
        }
    }

    // Django class-based views: methods named get, post, put, delete, patch
    let exact_methods = [
        ("get", "GET"),
        ("post", "POST"),
        ("put", "PUT"),
        ("delete", "DELETE"),
        ("patch", "PATCH"),
    ];

    // Only match exact method names when they're methods (contain :: separator)
    if id.contains("::") {
        for (exact, method) in &exact_methods {
            if name_lower == *exact {
                // Infer path from the class name (parent in the id)
                let parts: Vec<&str> = id.rsplitn(2, "::").collect();
                let parent = parts.last().unwrap_or(&"");
                let parent_name = parent.rsplit("::").next().unwrap_or(parent);
                let path = format!(
                    "/{}",
                    parent_name
                        .to_lowercase()
                        .trim_end_matches("view")
                        .trim_end_matches("viewset")
                        .trim_end_matches("handler")
                );
                return Some(Route {
                    method: method.to_string(),
                    path,
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: "django".to_string(),
                });
            }
        }
    }

    // Docstring mentions flask/fastapi/django route keywords
    if doc_lower.contains("flask") || doc_lower.contains("fastapi") || doc_lower.contains("django") {
        return Some(Route {
            method: "UNKNOWN".to_string(),
            path: format!("/{}", name_lower.replace('_', "/")),
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_python_framework(doc_lower),
        });
    }

    // Names ending in _handler, _view, _endpoint
    if name_lower.ends_with("_handler")
        || name_lower.ends_with("_view")
        || name_lower.ends_with("_endpoint")
        || name_lower.ends_with("_api")
    {
        let base = name_lower
            .trim_end_matches("_handler")
            .trim_end_matches("_view")
            .trim_end_matches("_endpoint")
            .trim_end_matches("_api");
        return Some(Route {
            method: infer_method_from_name(base),
            path: format!("/{}", base.replace('_', "/")),
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_python_framework(doc_lower),
        });
    }

    None
}

// ---------------------------------------------------------------------------
// JavaScript / TypeScript: Express, Koa, Hapi, Fastify, NestJS, Next.js
// ---------------------------------------------------------------------------

fn detect_js_ts_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    // Express-style: function names get, post, put, delete (as methods)
    let exact_methods = [
        ("get", "GET"),
        ("post", "POST"),
        ("put", "PUT"),
        ("delete", "DELETE"),
        ("patch", "PATCH"),
    ];

    if id.contains("::") {
        for (exact, method) in &exact_methods {
            if name_lower == *exact {
                let parts: Vec<&str> = id.rsplitn(2, "::").collect();
                let parent = parts.last().unwrap_or(&"");
                let parent_name = parent.rsplit("::").next().unwrap_or(parent);
                let path = format!(
                    "/{}",
                    parent_name
                        .to_lowercase()
                        .trim_end_matches("router")
                        .trim_end_matches("controller")
                );
                return Some(Route {
                    method: method.to_string(),
                    path,
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: detect_js_framework(file, doc_lower),
                });
            }
        }
    }

    // "handler" is a common name for serverless/API route handlers
    if name_lower == "handler" || name_lower == "default" {
        // Next.js API routes: the file path IS the route
        let method = if doc_lower.contains("post") {
            "POST"
        } else if doc_lower.contains("put") {
            "PUT"
        } else if doc_lower.contains("delete") {
            "DELETE"
        } else {
            "GET"
        };
        let path = infer_path_from_file(file);
        return Some(Route {
            method: method.to_string(),
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: if file.contains("/api/") || file.contains("pages/") { "nextjs".to_string() } else { detect_js_framework(file, doc_lower) },
        });
    }

    // NestJS-style: methods with names like getUsers, createUser, deleteUser
    let method_prefixes = [
        ("get", "GET"),
        ("find", "GET"),
        ("list", "GET"),
        ("fetch", "GET"),
        ("create", "POST"),
        ("add", "POST"),
        ("update", "PUT"),
        ("edit", "PUT"),
        ("remove", "DELETE"),
        ("delete", "DELETE"),
        ("handle", "UNKNOWN"),
    ];

    // Only if the file looks like a controller/route file
    let file_lower = file.to_lowercase();
    let is_route_file = file_lower.contains("controller")
        || file_lower.contains("route")
        || file_lower.contains("handler")
        || file_lower.contains("api")
        || file_lower.contains("endpoint");

    if is_route_file {
        for (prefix, method) in &method_prefixes {
            if name_lower.starts_with(prefix) && name_lower.len() > prefix.len() {
                let rest = &name_lower[prefix.len()..];
                // Ensure it's camelCase boundary (next char should be uppercase in original)
                if name.len() > prefix.len()
                    && name.as_bytes()[prefix.len()].is_ascii_uppercase()
                {
                    let path = format!("/{}", camel_to_path(rest));
                    return Some(Route {
                        method: method.to_string(),
                        path,
                        handler_id: id.to_string(),
                        file: file.to_string(),
                        framework: detect_js_framework(file, doc_lower),
                    });
                }
            }
        }
    }

    // Names ending with Handler, Controller, Route
    if name_lower.ends_with("handler")
        || name_lower.ends_with("controller")
        || name_lower.ends_with("route")
    {
        let base = name_lower
            .trim_end_matches("handler")
            .trim_end_matches("controller")
            .trim_end_matches("route");
        if !base.is_empty() {
            return Some(Route {
                method: "UNKNOWN".to_string(),
                path: format!("/{}", camel_to_path(base)),
                handler_id: id.to_string(),
                file: file.to_string(),
                framework: detect_js_framework(file, doc_lower),
            });
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Go: net/http, gorilla/mux, gin, echo, chi
// ---------------------------------------------------------------------------

fn detect_go_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    // Go convention: Handler suffix, ServeHTTP method
    if name == "ServeHTTP" {
        let parts: Vec<&str> = id.rsplitn(2, "::").collect();
        let parent = parts.last().unwrap_or(&"");
        let parent_name = parent.rsplit("::").next().unwrap_or(parent);
        let path = format!(
            "/{}",
            parent_name
                .to_lowercase()
                .trim_end_matches("handler")
        );
        return Some(Route {
            method: "UNKNOWN".to_string(),
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: "net/http".to_string(),
        });
    }

    // Functions ending in Handler
    if name.ends_with("Handler") && name.len() > "Handler".len() {
        let base = &name[..name.len() - "Handler".len()];
        let method = infer_method_from_name(&base.to_lowercase());
        let path = format!("/{}", camel_to_path(&base.to_lowercase()));
        return Some(Route {
            method,
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_go_framework(doc_lower),
        });
    }

    // Functions starting with Handle
    if name.starts_with("Handle") && name.len() > "Handle".len() {
        let base = &name["Handle".len()..];
        let method = infer_method_from_name(&base.to_lowercase());
        let path = format!("/{}", camel_to_path(&base.to_lowercase()));
        return Some(Route {
            method,
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_go_framework(doc_lower),
        });
    }

    // Docstring mentions http.HandleFunc or similar
    if doc_lower.contains("handlefunc")
        || doc_lower.contains("http.handle")
        || doc_lower.contains("gin.")
        || doc_lower.contains("echo.")
        || doc_lower.contains("chi.")
    {
        return Some(Route {
            method: "UNKNOWN".to_string(),
            path: format!("/{}", camel_to_path(name_lower)),
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_go_framework(doc_lower),
        });
    }

    // Go file naming: handler.go, routes.go, api.go
    let file_lower = file.to_lowercase();
    let is_handler_file = file_lower.ends_with("handler.go")
        || file_lower.ends_with("handlers.go")
        || file_lower.ends_with("routes.go")
        || file_lower.ends_with("api.go");

    if is_handler_file && name.starts_with(|c: char| c.is_uppercase()) {
        // Exported functions in handler files are likely handlers
        let method = infer_method_from_name(name_lower);
        let path = format!("/{}", camel_to_path(name_lower));
        return Some(Route {
            method,
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_go_framework(doc_lower),
        });
    }

    None
}

// ---------------------------------------------------------------------------
// Java: Spring Boot, JAX-RS, Micronaut, Quarkus
// ---------------------------------------------------------------------------

fn detect_java_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    // Spring-style: method names or docstrings containing Mapping
    if doc_lower.contains("mapping")
        || doc_lower.contains("@get")
        || doc_lower.contains("@post")
        || doc_lower.contains("@put")
        || doc_lower.contains("@delete")
        || doc_lower.contains("@patch")
        || doc_lower.contains("@requestmapping")
        || doc_lower.contains("@getmapping")
        || doc_lower.contains("@postmapping")
        || doc_lower.contains("@putmapping")
        || doc_lower.contains("@deletemapping")
    {
        let method = if doc_lower.contains("@post") || doc_lower.contains("@postmapping") {
            "POST"
        } else if doc_lower.contains("@put") || doc_lower.contains("@putmapping") {
            "PUT"
        } else if doc_lower.contains("@delete") || doc_lower.contains("@deletemapping") {
            "DELETE"
        } else if doc_lower.contains("@patch") || doc_lower.contains("@patchmapping") {
            "PATCH"
        } else {
            "GET"
        };

        let path = extract_path_from_text(doc_lower)
            .unwrap_or_else(|| format!("/{}", camel_to_path(name_lower)));

        return Some(Route {
            method: method.to_string(),
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_java_framework(doc_lower),
        });
    }

    // JAX-RS: @GET, @POST, @Path
    if doc_lower.contains("@path") || doc_lower.contains("jax-rs") || doc_lower.contains("javax.ws.rs") {
        let method = infer_method_from_name(name_lower);
        let path = extract_path_from_text(doc_lower)
            .unwrap_or_else(|| format!("/{}", camel_to_path(name_lower)));
        return Some(Route {
            method,
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: "jaxrs".to_string(),
        });
    }

    // File in a controller package
    let file_lower = file.to_lowercase();
    let is_controller_file = file_lower.contains("controller")
        || file_lower.contains("resource")
        || file_lower.contains("endpoint");

    if is_controller_file {
        // Methods in controller files that follow REST naming patterns
        let method_prefixes = [
            ("get", "GET"),
            ("find", "GET"),
            ("list", "GET"),
            ("create", "POST"),
            ("save", "POST"),
            ("update", "PUT"),
            ("delete", "DELETE"),
            ("remove", "DELETE"),
        ];

        for (prefix, method) in &method_prefixes {
            if name_lower.starts_with(prefix) && name.len() > prefix.len() {
                if name.as_bytes().get(prefix.len()).is_some_and(|b| b.is_ascii_uppercase()) {
                    let rest = &name[prefix.len()..];
                    return Some(Route {
                        method: method.to_string(),
                        path: format!("/{}", camel_to_path(&rest.to_lowercase())),
                        handler_id: id.to_string(),
                        file: file.to_string(),
                        framework: detect_java_framework(doc_lower),
                    });
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Rust: Actix-web, Axum, Rocket, Warp
// ---------------------------------------------------------------------------

fn detect_rust_route(
    id: &str,
    _name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    // Docstring/attribute-based: #[get], #[post], etc.
    if doc_lower.contains("#[get") || doc_lower.contains("#[post")
        || doc_lower.contains("#[put") || doc_lower.contains("#[delete")
        || doc_lower.contains("#[patch") || doc_lower.contains("actix")
        || doc_lower.contains("axum") || doc_lower.contains("rocket")
    {
        let method = if doc_lower.contains("#[post") || doc_lower.contains("post") {
            "POST"
        } else if doc_lower.contains("#[put") || doc_lower.contains("put") {
            "PUT"
        } else if doc_lower.contains("#[delete") || doc_lower.contains("delete") {
            "DELETE"
        } else if doc_lower.contains("#[patch") {
            "PATCH"
        } else {
            "GET"
        };

        let path = extract_path_from_text(doc_lower)
            .unwrap_or_else(|| format!("/{}", name_lower.replace('_', "/")));

        return Some(Route {
            method: method.to_string(),
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: detect_rust_framework(doc_lower),
        });
    }

    // Name-based patterns for handler files
    let file_lower = file.to_lowercase();
    let is_handler_file = file_lower.contains("handler")
        || file_lower.contains("route")
        || file_lower.contains("api");

    if is_handler_file {
        let method_prefixes = [
            ("get_", "GET"),
            ("post_", "POST"),
            ("put_", "PUT"),
            ("delete_", "DELETE"),
            ("create_", "POST"),
            ("update_", "PUT"),
            ("remove_", "DELETE"),
            ("list_", "GET"),
            ("handle_", "UNKNOWN"),
        ];

        for (prefix, method) in &method_prefixes {
            if name_lower.starts_with(prefix) {
                let rest = &name_lower[prefix.len()..];
                return Some(Route {
                    method: method.to_string(),
                    path: format!("/{}", rest.replace('_', "/")),
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: detect_rust_framework(doc_lower),
                });
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Generic fallback for other languages
// ---------------------------------------------------------------------------

fn detect_generic_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    _doc_lower: &str,
) -> Option<Route> {
    let file_lower = file.to_lowercase();

    // Generic: function in a route/handler/api/controller file
    let is_route_file = file_lower.contains("route")
        || file_lower.contains("handler")
        || file_lower.contains("controller")
        || file_lower.contains("endpoint");

    if !is_route_file {
        return None;
    }

    // Common HTTP handler name patterns
    let method_prefixes = [
        ("get_", "GET"),
        ("post_", "POST"),
        ("put_", "PUT"),
        ("delete_", "DELETE"),
        ("handle_", "UNKNOWN"),
    ];

    for (prefix, method) in &method_prefixes {
        if name_lower.starts_with(prefix) {
            let rest = &name_lower[prefix.len()..];
            return Some(Route {
                method: method.to_string(),
                path: format!("/{}", rest.replace('_', "/")),
                handler_id: id.to_string(),
                file: file.to_string(),
                framework: "generic".to_string(),
            });
        }
    }

    // Ends with Handler/handler
    if name.ends_with("Handler") || name.ends_with("handler") {
        let base = name
            .trim_end_matches("Handler")
            .trim_end_matches("handler");
        if !base.is_empty() {
            return Some(Route {
                method: "UNKNOWN".to_string(),
                path: format!("/{}", camel_to_path(&base.to_lowercase())),
                handler_id: id.to_string(),
                file: file.to_string(),
                framework: "generic".to_string(),
            });
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Ruby: Rails, Sinatra
// ---------------------------------------------------------------------------

fn detect_ruby_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    // Rails controller: file in app/controllers/, methods index/show/create/update/destroy
    let file_lower = file.to_lowercase();
    let is_rails_controller = file_lower.contains("app/controllers/")
        || file_lower.contains("app\\controllers\\");

    let rails_actions = [
        ("index", "GET"),
        ("show", "GET"),
        ("new", "GET"),
        ("create", "POST"),
        ("edit", "GET"),
        ("update", "PUT"),
        ("destroy", "DELETE"),
    ];

    if is_rails_controller {
        for (action, method) in &rails_actions {
            if name_lower == *action {
                let controller = file_lower
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches("_controller.rb")
                    .trim_end_matches(".rb");
                return Some(Route {
                    method: method.to_string(),
                    path: format!("/{}/{}", controller, if *action == "index" { "" } else { action }).trim_end_matches('/').to_string(),
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: "rails".to_string(),
                });
            }
        }
    }

    // Sinatra: docstring or file mentions sinatra
    if doc_lower.contains("sinatra") || file_lower.contains("sinatra") {
        let method = infer_method_from_name(name_lower);
        return Some(Route {
            method,
            path: format!("/{}", name_lower.replace('_', "/")),
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: "sinatra".to_string(),
        });
    }

    // Generic Ruby: _handler/_action/_endpoint suffix in route/api files
    if file_lower.contains("route") || file_lower.contains("api") || file_lower.contains("endpoint") {
        let suffixes = ["_handler", "_action", "_endpoint"];
        for suffix in &suffixes {
            if name_lower.ends_with(suffix) {
                let base = &name_lower[..name_lower.len() - suffix.len()];
                return Some(Route {
                    method: infer_method_from_name(base),
                    path: format!("/{}", base.replace('_', "/")),
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: "generic_ruby".to_string(),
                });
            }
        }
    }

    let _ = (name, doc_lower);
    None
}

// ---------------------------------------------------------------------------
// PHP: Laravel, Symfony, Slim
// ---------------------------------------------------------------------------

fn detect_php_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    let file_lower = file.to_lowercase();

    // Laravel: app/Http/Controllers/, RESTful resource methods
    let is_laravel_controller = file_lower.contains("http/controllers")
        || file_lower.contains("http\\controllers");

    let laravel_actions = [
        ("index", "GET"),
        ("show", "GET"),
        ("create", "GET"),
        ("store", "POST"),
        ("edit", "GET"),
        ("update", "PUT"),
        ("destroy", "DELETE"),
    ];

    if is_laravel_controller {
        for (action, method) in &laravel_actions {
            if name_lower == *action {
                return Some(Route {
                    method: method.to_string(),
                    path: format!("/{}", action),
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: "laravel".to_string(),
                });
            }
        }
    }

    // Symfony: docstring contains @Route or #[Route
    if doc_lower.contains("@route") || doc_lower.contains("#[route") || doc_lower.contains("symfony") {
        let method = infer_method_from_name(name_lower);
        let path = extract_path_from_text(doc_lower)
            .unwrap_or_else(|| format!("/{}", name_lower.replace('_', "/")));
        return Some(Route {
            method,
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: "symfony".to_string(),
        });
    }

    // Slim: docstring mentions slim
    if doc_lower.contains("slim") {
        let method = infer_method_from_name(name_lower);
        return Some(Route {
            method,
            path: format!("/{}", name_lower.replace('_', "/")),
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: "slim".to_string(),
        });
    }

    let _ = (name, doc_lower);
    None
}

// ---------------------------------------------------------------------------
// C#: ASP.NET Core
// ---------------------------------------------------------------------------

fn detect_csharp_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    let file_lower = file.to_lowercase();

    // ASP.NET: attribute routing in docstring
    if doc_lower.contains("[httpget")
        || doc_lower.contains("[httppost")
        || doc_lower.contains("[httpput")
        || doc_lower.contains("[httpdelete")
        || doc_lower.contains("[httppatch")
        || doc_lower.contains("[route(")
        || doc_lower.contains("apicontroller")
    {
        let method = if doc_lower.contains("[httppost") { "POST".to_string() }
            else if doc_lower.contains("[httpput") { "PUT".to_string() }
            else if doc_lower.contains("[httpdelete") { "DELETE".to_string() }
            else if doc_lower.contains("[httppatch") { "PATCH".to_string() }
            else { "GET".to_string() };
        let path = extract_path_from_text(doc_lower)
            .unwrap_or_else(|| format!("/{}", camel_to_path(name_lower)));
        return Some(Route {
            method,
            path,
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: "aspnet".to_string(),
        });
    }

    // Controller file: file ends with Controller.cs
    if file_lower.ends_with("controller.cs") || file_lower.contains("controllers/") || file_lower.contains("controllers\\") {
        let method_prefixes = [
            ("Get", "GET"), ("List", "GET"), ("Find", "GET"),
            ("Post", "POST"), ("Create", "POST"), ("Add", "POST"),
            ("Put", "PUT"), ("Update", "PUT"),
            ("Delete", "DELETE"), ("Remove", "DELETE"),
            ("Patch", "PATCH"),
        ];
        for (prefix, method) in &method_prefixes {
            if name.starts_with(prefix) && name.len() > prefix.len() {
                let rest = &name[prefix.len()..];
                return Some(Route {
                    method: method.to_string(),
                    path: format!("/{}", camel_to_path(&rest.to_lowercase())),
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: "aspnet".to_string(),
                });
            }
        }
    }

    let _ = (name_lower, doc_lower);
    None
}

// ---------------------------------------------------------------------------
// Elixir: Phoenix
// ---------------------------------------------------------------------------

fn detect_elixir_route(
    id: &str,
    name: &str,
    name_lower: &str,
    file: &str,
    doc_lower: &str,
) -> Option<Route> {
    let file_lower = file.to_lowercase();

    // Phoenix controller: file in lib/*/controllers/ or web/controllers/
    let is_phoenix_controller = file_lower.contains("/controllers/")
        && (file_lower.ends_with("_controller.ex") || file_lower.ends_with("_controller.exs"));

    let phoenix_actions = [
        ("index", "GET"),
        ("show", "GET"),
        ("new", "GET"),
        ("create", "POST"),
        ("edit", "GET"),
        ("update", "PUT"),
        ("delete", "DELETE"),
    ];

    if is_phoenix_controller {
        for (action, method) in &phoenix_actions {
            if name_lower == *action {
                return Some(Route {
                    method: method.to_string(),
                    path: format!("/{}", action),
                    handler_id: id.to_string(),
                    file: file.to_string(),
                    framework: "phoenix".to_string(),
                });
            }
        }
    }

    // Plug: docstring or name references plug
    if doc_lower.contains("plug") || doc_lower.contains("phoenix") {
        let method = infer_method_from_name(name_lower);
        return Some(Route {
            method,
            path: format!("/{}", name_lower.replace('_', "/")),
            handler_id: id.to_string(),
            file: file.to_string(),
            framework: if doc_lower.contains("phoenix") { "phoenix".to_string() } else { "plug".to_string() },
        });
    }

    let _ = (name, doc_lower);
    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Lang {
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    Rust,
    Ruby,
    Php,
    CSharp,
    Elixir,
    Other,
}

fn language_from_file(file: &str) -> Lang {
    if file.ends_with(".py") {
        Lang::Python
    } else if file.ends_with(".js") || file.ends_with(".jsx") || file.ends_with(".mjs") {
        Lang::JavaScript
    } else if file.ends_with(".ts") || file.ends_with(".tsx") {
        Lang::TypeScript
    } else if file.ends_with(".go") {
        Lang::Go
    } else if file.ends_with(".java") || file.ends_with(".kt") || file.ends_with(".scala") {
        Lang::Java
    } else if file.ends_with(".rs") {
        Lang::Rust
    } else if file.ends_with(".rb") {
        Lang::Ruby
    } else if file.ends_with(".php") {
        Lang::Php
    } else if file.ends_with(".cs") {
        Lang::CSharp
    } else if file.ends_with(".ex") || file.ends_with(".exs") {
        Lang::Elixir
    } else {
        Lang::Other
    }
}

fn detect_python_framework(doc_lower: &str) -> String {
    if doc_lower.contains("fastapi") { "fastapi".to_string() }
    else if doc_lower.contains("flask") || doc_lower.contains("@app.") || doc_lower.contains("@blueprint.") { "flask".to_string() }
    else if doc_lower.contains("django") { "django".to_string() }
    else if doc_lower.contains("starlette") { "starlette".to_string() }
    else if doc_lower.contains("tornado") { "tornado".to_string() }
    else if doc_lower.contains("aiohttp") { "aiohttp".to_string() }
    else { "generic_python".to_string() }
}

fn detect_js_framework(file: &str, doc_lower: &str) -> String {
    let file_lower = file.to_lowercase();
    if doc_lower.contains("nestjs") || doc_lower.contains("@controller") || doc_lower.contains("@get(") || doc_lower.contains("@post(") { "nestjs".to_string() }
    else if file_lower.contains("pages/api/") || file_lower.contains("app/api/") { "nextjs".to_string() }
    else if doc_lower.contains("fastify") { "fastify".to_string() }
    else if doc_lower.contains("koa") { "koa".to_string() }
    else if doc_lower.contains("hapi") { "hapi".to_string() }
    else if doc_lower.contains("express") { "express".to_string() }
    else { "generic_js".to_string() }
}

fn detect_go_framework(doc_lower: &str) -> String {
    if doc_lower.contains("gin.") || doc_lower.contains("gin ") { "gin".to_string() }
    else if doc_lower.contains("echo.") { "echo".to_string() }
    else if doc_lower.contains("chi.") { "chi".to_string() }
    else if doc_lower.contains("fiber") { "fiber".to_string() }
    else if doc_lower.contains("mux") || doc_lower.contains("gorilla") { "gorilla/mux".to_string() }
    else { "net/http".to_string() }
}

fn detect_java_framework(doc_lower: &str) -> String {
    if doc_lower.contains("@getmapping") || doc_lower.contains("@postmapping")
        || doc_lower.contains("@requestmapping") || doc_lower.contains("@putmapping")
        || doc_lower.contains("@deletemapping") || doc_lower.contains("@patchmapping") { "spring".to_string() }
    else if doc_lower.contains("@path") || doc_lower.contains("jax-rs") || doc_lower.contains("javax.ws.rs") { "jaxrs".to_string() }
    else if doc_lower.contains("micronaut") { "micronaut".to_string() }
    else if doc_lower.contains("quarkus") { "quarkus".to_string() }
    else if doc_lower.contains("ktor") { "ktor".to_string() }
    else { "spring".to_string() }
}

fn detect_rust_framework(doc_lower: &str) -> String {
    if doc_lower.contains("actix") { "actix".to_string() }
    else if doc_lower.contains("axum") { "axum".to_string() }
    else if doc_lower.contains("rocket") || doc_lower.contains("#[get") || doc_lower.contains("#[post") { "rocket".to_string() }
    else if doc_lower.contains("warp") { "warp".to_string() }
    else if doc_lower.contains("tide") { "tide".to_string() }
    else { "generic_rust".to_string() }
}

fn detect_framework_from_docstring(doc_lower: &str) -> String {
    if doc_lower.contains("flask") || doc_lower.contains("@app.") { "flask".to_string() }
    else if doc_lower.contains("fastapi") { "fastapi".to_string() }
    else if doc_lower.contains("django") { "django".to_string() }
    else if doc_lower.contains("express") { "express".to_string() }
    else if doc_lower.contains("nestjs") { "nestjs".to_string() }
    else if doc_lower.contains("spring") || doc_lower.contains("mapping") { "spring".to_string() }
    else if doc_lower.contains("actix") { "actix".to_string() }
    else if doc_lower.contains("axum") { "axum".to_string() }
    else if doc_lower.contains("rocket") { "rocket".to_string() }
    else if doc_lower.contains("gin.") { "gin".to_string() }
    else if doc_lower.contains("rails") { "rails".to_string() }
    else if doc_lower.contains("laravel") { "laravel".to_string() }
    else if doc_lower.contains("phoenix") { "phoenix".to_string() }
    else if doc_lower.contains("handlefunc") || doc_lower.contains("http.handle") { "net/http".to_string() }
    else { "generic".to_string() }
}

/// Try to extract a URL path (e.g., /users/{id}) from text.
fn extract_path_from_text(text: &str) -> Option<String> {
    // Look for patterns like "/something" or '/something'
    for delim in ['"', '\''] {
        if let Some(start) = text.find(&format!("{}/", delim)) {
            let path_start = start + 1; // skip the delimiter
            if let Some(end) = text[path_start..].find(delim) {
                let path = &text[path_start..path_start + end];
                if path.starts_with('/') && path.len() > 1 {
                    return Some(path.to_string());
                }
            }
        }
    }

    // Look for unquoted /path patterns (e.g., in docstrings: "GET /users")
    for word in text.split_whitespace() {
        if word.starts_with('/') && word.len() > 1 && !word.starts_with("//") {
            return Some(word.to_string());
        }
    }

    None
}

/// Infer HTTP method from a name (e.g., "create_user" -> "POST").
fn infer_method_from_name(name: &str) -> String {
    if name.starts_with("get")
        || name.starts_with("list")
        || name.starts_with("find")
        || name.starts_with("fetch")
        || name.starts_with("read")
        || name.starts_with("show")
        || name.starts_with("index")
    {
        "GET".to_string()
    } else if name.starts_with("create")
        || name.starts_with("add")
        || name.starts_with("post")
        || name.starts_with("save")
        || name.starts_with("new")
    {
        "POST".to_string()
    } else if name.starts_with("update")
        || name.starts_with("put")
        || name.starts_with("edit")
        || name.starts_with("modify")
    {
        "PUT".to_string()
    } else if name.starts_with("delete")
        || name.starts_with("remove")
        || name.starts_with("destroy")
    {
        "DELETE".to_string()
    } else if name.starts_with("patch") {
        "PATCH".to_string()
    } else {
        "UNKNOWN".to_string()
    }
}

/// Convert camelCase to a URL path segment: "userProfile" -> "user/profile".
fn camel_to_path(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.char_indices() {
        if c.is_uppercase() && i > 0 {
            result.push('/');
            result.push(c.to_lowercase().next().unwrap_or(c));
        } else {
            result.push(c);
        }
    }
    // Also convert underscores to slashes
    result.replace('_', "/")
}

/// Infer a route path from the file path (useful for Next.js API routes, etc.).
fn infer_path_from_file(file: &str) -> String {
    // Next.js: pages/api/users/[id].ts -> /api/users/:id
    // Also: app/api/users/route.ts -> /api/users
    let normalized = file
        .replace('\\', "/")
        .to_lowercase();

    // Try to extract the API route part
    if let Some(api_idx) = normalized.find("/api/") {
        let path_part = &file[api_idx..];
        let cleaned = path_part
            .trim_end_matches(".ts")
            .trim_end_matches(".tsx")
            .trim_end_matches(".js")
            .trim_end_matches(".jsx")
            .trim_end_matches("/route")
            .trim_end_matches("/index");
        // Convert [param] to :param
        let result = cleaned
            .replace('[', ":")
            .replace(']', "");
        return result;
    }

    // Fallback: use the file stem
    let stem = file
        .rsplit('/')
        .next()
        .unwrap_or(file)
        .trim_end_matches(".ts")
        .trim_end_matches(".tsx")
        .trim_end_matches(".js")
        .trim_end_matches(".jsx")
        .trim_end_matches(".py")
        .trim_end_matches(".go")
        .trim_end_matches(".rs");

    format!("/{}", stem.to_lowercase())
}

/// Format routes as a displayable string.
pub fn format_routes(routes: &[Route]) -> String {
    if routes.is_empty() {
        return "No HTTP routes detected.".to_string();
    }

    let mut out = format!("Detected {} HTTP route(s):\n\n", routes.len());

    let mut current_file = "";
    for route in routes {
        if route.file != current_file {
            current_file = &route.file;
            out.push_str(&format!("  {}:\n", current_file));
        }
        out.push_str(&format!(
            "    {:>7} {:30} [{:15}] [{}]\n",
            route.method, route.path, route.framework, route.handler_id
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_python_get_prefix() {
        let route = detect_route_from_symbol(
            "views.py::get_users",
            "get_users",
            "views.py",
            "",
        );
        assert!(route.is_some());
        let r = route.unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/users");
    }

    #[test]
    fn test_python_post_prefix() {
        let route = detect_route_from_symbol(
            "views.py::post_order",
            "post_order",
            "views.py",
            "",
        );
        assert!(route.is_some());
        let r = route.unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/order");
    }

    #[test]
    fn test_python_handler_suffix() {
        let route = detect_route_from_symbol(
            "views.py::user_handler",
            "user_handler",
            "views.py",
            "",
        );
        assert!(route.is_some());
        let r = route.unwrap();
        assert_eq!(r.path, "/user");
    }

    #[test]
    fn test_go_handler_suffix() {
        let route = detect_route_from_symbol(
            "api.go::UsersHandler",
            "UsersHandler",
            "api.go",
            "",
        );
        assert!(route.is_some());
        let r = route.unwrap();
        assert!(r.path.contains("users"));
    }

    #[test]
    fn test_go_serve_http() {
        let route = detect_route_from_symbol(
            "server.go::MyHandler::ServeHTTP",
            "ServeHTTP",
            "server.go",
            "",
        );
        assert!(route.is_some());
    }

    #[test]
    fn test_js_handler() {
        let route = detect_route_from_symbol(
            "api/users.ts::handler",
            "handler",
            "api/users.ts",
            "",
        );
        assert!(route.is_some());
    }

    #[test]
    fn test_docstring_route() {
        let route = detect_route_from_symbol(
            "app.py::list_items",
            "list_items",
            "app.py",
            "GET /api/items endpoint",
        );
        assert!(route.is_some());
        let r = route.unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/api/items");
    }

    #[test]
    fn test_java_controller_file() {
        let route = detect_route_from_symbol(
            "UserController.java::UserController::getUsers",
            "getUsers",
            "com/example/controller/UserController.java",
            "",
        );
        assert!(route.is_some());
        let r = route.unwrap();
        assert_eq!(r.method, "GET");
    }

    #[test]
    fn test_no_false_positive_regular_function() {
        let route = detect_route_from_symbol(
            "utils.py::format_string",
            "format_string",
            "utils.py",
            "",
        );
        assert!(route.is_none());
    }

    #[test]
    fn test_extract_path_from_text() {
        assert_eq!(
            extract_path_from_text("route \"/api/users\""),
            Some("/api/users".to_string())
        );
        assert_eq!(
            extract_path_from_text("GET /api/items endpoint"),
            Some("/api/items".to_string())
        );
    }

    #[test]
    fn test_camel_to_path() {
        assert_eq!(camel_to_path("users"), "users");
        assert_eq!(camel_to_path("user_profile"), "user/profile");
    }
}
