SELECT to_timestamp(started_at_unix_seconds) AS gateway_started,
       list_calls, openapi_fetches, access_reviews
  FROM axiom_gateway_stats('prod');
