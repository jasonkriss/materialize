# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Test behavior that is specific to Kafka Avro Sinks with ENVELOPE DEBEZIUM

# Test a basic sink with multiple rows.

> CREATE VIEW data (a, b) AS VALUES (1, 1), (2, 1), (3, 1), (1, 2)

> CREATE SINK data_sink FROM data
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'data-sink'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

$ kafka-verify format=avro sink=materialize.public.data_sink sort-messages=true
{"before": null, "after": {"row": {"a": 1, "b": 1}}}
{"before": null, "after": {"row": {"a": 1, "b": 2}}}
{"before": null, "after": {"row": {"a": 2, "b": 1}}}
{"before": null, "after": {"row": {"a": 3, "b": 1}}}

# More complex sinks, with multiple keys and/or a consistency topic. We test
# all the possible combinations of user-specified sink key and
# natural (primary) relation key.

$ set schema={
    "type": "record",
    "name": "envelope",
    "fields": [
      {
        "name": "before",
        "type": [
          {
            "name": "row",
            "type": "record",
            "fields": [
              {"name": "a", "type": "long"},
              {"name": "b", "type": "long"}
            ]
          },
          "null"
        ]
      },
      { "name": "after", "type": ["row", "null"] }
    ]
  }

$ set trxschemakey={
      "name": "io.debezium.connector.common.TransactionMetadataKey",
      "type": "record",
      "fields": [
          {
              "name": "id",
              "type": "string"
          }
      ]
  }

$ set trxschema={
    "type":"record", "name":"TransactionMetadataValue", "namespace":"io.debezium.connector.common",
    "fields":[
    {"name":"status","type":"string"},
    {"name":"id","type": "string"},
    {"name": "event_count",
    "type": ["null", "long"],
    "default": null
    },
    {"name":"data_collections","type":["null",{"type":"array",
    "items": {"type":"record",
    "name":"ConnectDefault",
    "namespace":"io.confluent.connect.Avro",
    "fields": [ {
    "name": "data_collection",
    "type": "string"
    },
    {
    "name": "event_count",
    "type": "long" }]}}],
    "default": null}],
    "connect.name": "io.debezium.connector.common.TransactionMetadataValue"
    }

$ kafka-create-topic topic=consistency
$ kafka-create-topic topic=input

# first create all the sinks, then ingest data, to ensure that
# input is processed in consistency batches and not all at once

> CREATE MATERIALIZED SOURCE input
  FROM KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-input-${testdrive.seed}'
    WITH (consistency_topic = 'testdrive-consistency-${testdrive.seed}')
  FORMAT AVRO USING SCHEMA '${schema}' ENVELOPE DEBEZIUM

> CREATE SINK non_keyed_sink FROM input
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'non-keyed-sink'
  WITH (consistency_topic = 'non-keyed-sink-consistency') FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE VIEW max_view AS SELECT a, MAX(b) as b FROM input GROUP BY a

# the sinked relation has the natural primary key (a)

> CREATE SINK non_keyed_sink_of_keyed_relation FROM input
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'non-keyed-sink-of-keyed-relation'
  WITH (consistency_topic = 'non-keyed-sink-of-keyed-relation-consistency') FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE SINK keyed_sink FROM input
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'keyed-sink' KEY (a)
  WITH (consistency_topic = 'keyed-sink-consistency') FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE SINK keyed_sink_of_keyed_relation FROM input
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'keyed-sink-of-keyed-relation' KEY (b)
  WITH (consistency_topic = 'keyed-sink-of-keyed-relation-consistency') FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

> CREATE SINK multi_keyed_sink FROM input
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'multi-keyed-sink' KEY (b, a)
  WITH (consistency_topic = 'multi-keyed-sink-consistency') FORMAT AVRO
  USING CONFLUENT SCHEMA REGISTRY '${testdrive.schema-registry-url}'

$ kafka-ingest format=avro topic=input schema=${schema} timestamp=1
{"before": null, "after": {"row": {"a": 1, "b": 1}}}
{"before": null, "after": {"row": {"a": 2, "b": 2}}}

$ kafka-ingest format=avro topic=input schema=${schema} timestamp=1
{"before": null, "after": {"row": {"a": 3, "b": 1}}}
{"before": null, "after": {"row": {"a": 4, "b": 2}}}
{"before": null, "after": {"row": {"a": 1, "b": 7}}}

$ kafka-ingest format=avro topic=consistency timestamp=1 schema=${trxschema}
{"status":"BEGIN","id":"1","event_count":null,"data_collections":null}
{"status":"END","id":"1","event_count":{"long": 2},"data_collections":{"array": [{"event_count": 2, "data_collection": "testdrive-input-${testdrive.seed}"}]}}
{"status":"BEGIN","id":"2","event_count":null,"data_collections":null}
{"status":"END","id":"2","event_count":{"long": 2},"data_collections":{"array": [{"event_count": 2, "data_collection": "testdrive-input-${testdrive.seed}"}]}}
{"status":"BEGIN","id":"3","event_count":null,"data_collections":null}
{"status":"END","id":"3","event_count":{"long": 1},"data_collections":{"array": [{"event_count": 1, "data_collection": "testdrive-input-${testdrive.seed}"}]}}

> SELECT * FROM input;
a  b
------
1  1
2  2
3  1
4  2
1  7

# Compare sorted messages within each transaction. We know that messages of one
# transaction appear together as one "bundle" in the output. But there is no
# guarantee on the order within a transaction.

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink sort-messages=true
{"before": null, "after": {"row": {"a": 1, "b": 1}}, "transaction": {"id": "1"}}
{"before": null, "after": {"row": {"a": 2, "b": 2}}, "transaction": {"id": "1"}}

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink sort-messages=true
{"before": null, "after": {"row": {"a": 3, "b": 1}}, "transaction": {"id": "2"}}
{"before": null, "after": {"row": {"a": 4, "b": 2}}, "transaction": {"id": "2"}}

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink sort-messages=true
{"before": null, "after": {"row": {"a": 1, "b": 7}}, "transaction": {"id": "3"}}

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink consistency=debezium
{"id": "1", "status": "BEGIN", "event_count": null}
{"id": "1", "status": "END", "event_count": {"long": 2}}
{"id": "2", "status": "BEGIN", "event_count": null}
{"id": "2", "status": "END", "event_count": {"long": 2}}
{"id": "3", "status": "BEGIN", "event_count": null}
{"id": "3", "status": "END", "event_count": {"long": 1}}

# Again, compare split by transaction. See comment just above.

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink_of_keyed_relation sort-messages=true
{"before": null, "after": {"row": {"a": 1, "b": 1}}, "transaction": {"id": "1"}}
{"before": null, "after": {"row": {"a": 2, "b": 2}}, "transaction": {"id": "1"}}

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink_of_keyed_relation sort-messages=true
{"before": null, "after": {"row": {"a": 3, "b": 1}}, "transaction": {"id": "2"}}
{"before": null, "after": {"row": {"a": 4, "b": 2}}, "transaction": {"id": "2"}}

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink_of_keyed_relation sort-messages=true
{"before": null, "after": {"row": {"a": 1, "b": 7}}, "transaction": {"id": "3"}}

$ kafka-verify format=avro sink=materialize.public.non_keyed_sink_of_keyed_relation consistency=debezium
{"id": "1", "status": "BEGIN", "event_count": null}
{"id": "1", "status": "END", "event_count": {"long": 2}}
{"id": "2", "status": "BEGIN", "event_count": null}
{"id": "2", "status": "END", "event_count": {"long": 2}}
{"id": "3", "status": "BEGIN", "event_count": null}
{"id": "3", "status": "END", "event_count": {"long": 1}}

# Again, compare split by transaction. See comment just above.

$ kafka-verify format=avro sink=materialize.public.keyed_sink sort-messages=true
{"a": 1} {"before": null, "after": {"row": {"a": 1, "b": 1}}, "transaction": {"id": "1"}}
{"a": 2} {"before": null, "after": {"row": {"a": 2, "b": 2}}, "transaction": {"id": "1"}}

$ kafka-verify format=avro sink=materialize.public.keyed_sink sort-messages=true
{"a": 3} {"before": null, "after": {"row": {"a": 3, "b": 1}}, "transaction": {"id": "2"}}
{"a": 4} {"before": null, "after": {"row": {"a": 4, "b": 2}}, "transaction": {"id": "2"}}

$ kafka-verify format=avro sink=materialize.public.keyed_sink sort-messages=true
{"a": 1} {"before": null, "after": {"row": {"a": 1, "b": 7}}, "transaction": {"id": "3"}}

$ kafka-verify format=avro sink=materialize.public.keyed_sink consistency=debezium
{"id": "1", "status": "BEGIN", "event_count": null}
{"id": "1", "status": "END", "event_count": {"long": 2}}
{"id": "2", "status": "BEGIN", "event_count": null}
{"id": "2", "status": "END", "event_count": {"long": 2}}
{"id": "3", "status": "BEGIN", "event_count": null}
{"id": "3", "status": "END", "event_count": {"long": 1}}

# Again, compare split by transaction. See comment just above.

$ kafka-verify format=avro sink=materialize.public.keyed_sink_of_keyed_relation sort-messages=true
{"b": 1} {"before": null, "after": {"row": {"a": 1, "b": 1}}, "transaction": {"id": "1"}}
{"b": 2} {"before": null, "after": {"row": {"a": 2, "b": 2}}, "transaction": {"id": "1"}}

$ kafka-verify format=avro sink=materialize.public.keyed_sink_of_keyed_relation sort-messages=true
{"b": 1} {"before": null, "after": {"row": {"a": 3, "b": 1}}, "transaction": {"id": "2"}}
{"b": 2} {"before": null, "after": {"row": {"a": 4, "b": 2}}, "transaction": {"id": "2"}}

$ kafka-verify format=avro sink=materialize.public.keyed_sink_of_keyed_relation sort-messages=true
{"b": 7} {"before": null, "after": {"row": {"a": 1, "b": 7}}, "transaction": {"id": "3"}}

$ kafka-verify format=avro sink=materialize.public.keyed_sink_of_keyed_relation consistency=debezium
{"id": "1", "status": "BEGIN", "event_count": null}
{"id": "1", "status": "END", "event_count": {"long": 2}}
{"id": "2", "status": "BEGIN", "event_count": null}
{"id": "2", "status": "END", "event_count": {"long": 2}}
{"id": "3", "status": "BEGIN", "event_count": null}
{"id": "3", "status": "END", "event_count": {"long": 1}}

# Again, compare split by transaction. See comment just above.

$ kafka-verify format=avro sink=materialize.public.multi_keyed_sink sort-messages=true
{"b": 1, "a": 1} {"before": null, "after": {"row": {"a": 1, "b": 1}}, "transaction": {"id": "1"}}
{"b": 2, "a": 2} {"before": null, "after": {"row": {"a": 2, "b": 2}}, "transaction": {"id": "1"}}

$ kafka-verify format=avro sink=materialize.public.multi_keyed_sink sort-messages=true
{"b": 1, "a": 3} {"before": null, "after": {"row": {"a": 3, "b": 1}}, "transaction": {"id": "2"}}
{"b": 2, "a": 4} {"before": null, "after": {"row": {"a": 4, "b": 2}}, "transaction": {"id": "2"}}

$ kafka-verify format=avro sink=materialize.public.multi_keyed_sink sort-messages=true
{"b": 7, "a": 1} {"before": null, "after": {"row": {"a": 1, "b": 7}}, "transaction": {"id": "3"}}

$ kafka-verify format=avro sink=materialize.public.multi_keyed_sink consistency=debezium
{"id": "1", "status": "BEGIN", "event_count": null}
{"id": "1", "status": "END", "event_count": {"long": 2}}
{"id": "2", "status": "BEGIN", "event_count": null}
{"id": "2", "status": "END", "event_count": {"long": 2}}
{"id": "3", "status": "BEGIN", "event_count": null}
{"id": "3", "status": "END", "event_count": {"long": 1}}
