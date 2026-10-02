package com.example.app.api;

import java.util.List;
import retrofit2.Call;
import retrofit2.http.DELETE;
import retrofit2.http.GET;
import retrofit2.http.Path;

public interface CrateApi {
    @GET("/v1/crates")
    Call<List<Crate>> list();

    @DELETE("/v1/crates/{crateId}")
    Call<Void> delete(@Path("crateId") String crateId);
}
