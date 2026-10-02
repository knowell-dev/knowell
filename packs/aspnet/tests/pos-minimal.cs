var builder = WebApplication.CreateBuilder(args);
var app = builder.Build();

app.MapGet("/v1/lanes/{laneId}", (string laneId) => Results.Ok(laneId));
app.MapPost("/v1/lanes", () => Results.Created("/v1/lanes/1", null));

app.Run();
