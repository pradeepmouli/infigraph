import io.ktor.server.routing.*
import io.ktor.server.response.*

fun Application.configureRouting() {
    routing {
        get("/api/users") { call.respond(listUsers()) }
        post("/api/users") { call.respond(createUser()) }
        get("/api/users/{id}") { call.respond(getUser()) }
        put("/api/users/{id}") { call.respond(updateUser()) }
        delete("/api/users/{id}") { call.respond(deleteUser()) }
        route("/api/orders") {
            get("/") { call.respond(listOrders()) }
            post("/") { call.respond(createOrder()) }
        }
    }
}
