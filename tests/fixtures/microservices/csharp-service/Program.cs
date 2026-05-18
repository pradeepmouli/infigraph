var builder = WebApplication.CreateBuilder(args);
var app = builder.Build();

app.MapGet("/api/users", () => Results.Ok(new List<User>()));
app.MapPost("/api/users", (User user) => Results.Created($"/api/users/{user.Id}", user));
app.MapGet("/api/users/{id}", (int id) => Results.Ok(new User()));
app.MapPut("/api/users/{id}", (int id, User user) => Results.Ok(user));
app.MapDelete("/api/users/{id}", (int id) => Results.NoContent());

var orders = app.MapGroup("/api/orders");
orders.MapGet("/", () => Results.Ok(new List<Order>()));
orders.MapPost("/", (Order order) => Results.Created());

app.Run();
