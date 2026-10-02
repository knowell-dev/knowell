package com.example.app.api;

import javax.ws.rs.GET;
import javax.ws.rs.Path;

// JAX-RS marker annotations (no path argument) declare a server resource.
@Path("/v1/crates")
public class CrateResource {
    @GET
    public String list() {
        return "[]";
    }
}
