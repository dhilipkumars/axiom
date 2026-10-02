SELECT axiom_quantity('100m')  AS "100m",
       axiom_quantity('128Mi') AS "128Mi",
       axiom_quantity('1M') = axiom_quantity('1Mi') AS "1M = 1Mi";
