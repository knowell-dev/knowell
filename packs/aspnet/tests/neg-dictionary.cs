using System.Collections.Generic;

namespace Example.Fleet;

public class RouteTable
{
    private readonly Dictionary<string, string> _routes = new();

    // Strings that look like routes outside routing attributes and Map* calls.
    public void Add(string key) => _routes[key] = "/v1/trucks";

    public string Lookup() => _routes.GetValueOrDefault("/v1/lanes") ?? "";
}
