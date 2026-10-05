SELECT pid, backend_type, backend_start
  FROM pg_stat_activity
 WHERE backend_type = 'axiom gateway pinger';
