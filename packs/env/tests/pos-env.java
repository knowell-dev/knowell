package com.example.depot;

public class Config {
    public static String region() {
        return System.getenv("DEPOT_REGION");
    }
}
