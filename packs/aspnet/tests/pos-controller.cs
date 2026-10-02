using Microsoft.AspNetCore.Mvc;

namespace Example.Fleet;

[ApiController]
[Route("api/[controller]")]
public class TrucksController : ControllerBase
{
    [HttpGet("{truckId}")]
    public IActionResult Get(string truckId) => Ok(truckId);

    [HttpPost]
    public IActionResult Create() => Ok();

    [HttpDelete("{truckId:guid}")]
    public IActionResult Delete(Guid truckId) => NoContent();
}
