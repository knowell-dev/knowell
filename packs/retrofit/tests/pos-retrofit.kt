package com.example.app.api

import retrofit2.http.Body
import retrofit2.http.GET
import retrofit2.http.POST
import retrofit2.http.Path

interface PalletApi {
    @GET("v1/pallets/{palletId}")
    suspend fun get(@Path("palletId") palletId: String): Pallet

    @POST("v1/pallets")
    suspend fun create(@Body pallet: Pallet): Pallet
}
