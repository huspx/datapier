use super::*;

impl App {
    pub(super) fn transfer_spec(&self) -> std::result::Result<Value, (String, String)> {
        let get = |key: &str| self.transfer_value(key).trim().to_owned();
        let mode = get("mode");
        let provider = get("provider");
        if matches!(mode.as_str(), "sql_dump" | "sql_restore") && provider != "mysql" {
            return Err(("provider".into(), "SQL 备份恢复仅用于 MySQL".into()));
        }
        if get("include-schema") != "true" && get("include-data") != "true" {
            return Err(("include-schema".into(), "至少选择结构或数据".into()));
        }
        let endpoint = |prefix: &str| -> std::result::Result<Value, (String, String)> {
            if self.workbench.transfer.project_mode {
                let alias = get(&format!("{prefix}-alias"));
                if alias.is_empty() {
                    return Err((format!("{prefix}-alias"), "请选择项目连接别名".into()));
                }
                return Ok(json!({"connection_alias":alias}));
            }
            let profile = get(&format!("{prefix}-profile"));
            let id = Uuid::parse_str(&profile).map_err(|_| {
                (
                    format!("{prefix}-profile"),
                    if prefix == "destination" {
                        "请选择目标连接"
                    } else {
                        "请选择来源连接"
                    }
                    .into(),
                )
            })?;
            let database = get(&format!("{prefix}-database"));
            let redis = get(&format!("{prefix}-redis-db"));
            let redis_db = if redis.is_empty() {
                None
            } else {
                Some(redis.parse::<u32>().map_err(|_| {
                    (
                        format!("{prefix}-redis-db"),
                        "Redis DB 必须为非负整数".into(),
                    )
                })?)
            };
            Ok(
                json!({"profile_id":id,"database":if database.is_empty(){None}else{Some(database)},"redis_db":redis_db}),
            )
        };
        let source = if matches!(mode.as_str(), "copy" | "export" | "sql_dump") {
            Some(endpoint("source")?)
        } else {
            None
        };
        let destination = if matches!(mode.as_str(), "copy" | "import" | "sql_restore") {
            Some(endpoint("destination")?)
        } else {
            None
        };
        let format = get("format");
        let mut inputs = Vec::new();
        if matches!(mode.as_str(), "import" | "sql_restore") {
            for i in 0..self.workbench.transfer.files {
                let path = get(&format!("file-{i}"));
                if path.is_empty() {
                    return Err((format!("file-{i}"), "输入文件路径不能为空".into()));
                }
                let table = get(&format!("file-target-{i}"));
                let database = get("destination-database");
                let target = if table.is_empty() {
                    Value::Null
                } else {
                    json!({"database":database,"name":table,"kind":"table","key_b64":null})
                };
                let mappings = parse_mapping(&get(&format!("file-mapping-{i}")))
                    .map_err(|e| (format!("file-mapping-{i}"), e))?;
                if mappings
                    .values()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != mappings.len()
                {
                    return Err((format!("file-mapping-{i}"), "目标列为空或重复".into()));
                }
                let range = get(&format!("file-range-{i}"));
                let ranges: Vec<Value> = if range.is_empty() {
                    Vec::new()
                } else {
                    range.split(',').map(|part| {
                        let (first, last) = part.trim().split_once('-').ok_or("记录范围使用 起始-结束")?;
                        Ok(json!({"first":first.trim().parse::<u64>().map_err(|_| "无效记录范围")?,"last":last.trim().parse::<u64>().map_err(|_| "无效记录范围")?}))
                    }).collect::<std::result::Result<_, &str>>().map_err(|e| (format!("file-range-{i}"), e.to_owned()))?
                };
                if ranges.iter().any(|r| {
                    r["first"].as_u64() == Some(0) || r["last"].as_u64() < r["first"].as_u64()
                }) {
                    return Err((
                        format!("file-range-{i}"),
                        "记录范围须从 1 开始且结束不早于起始".into(),
                    ));
                }
                let types = parse_types(&get(&format!("file-types-{i}")))
                    .map_err(|e| (format!("file-types-{i}"), e))?;
                let mut defaults = BTreeMap::new();
                for column in &types {
                    let name = column["name"].as_str().unwrap_or("");
                    let kind = self.transfer_value(&column_default_key(i, name, "kind"));
                    if kind.is_empty() {
                        continue;
                    }
                    let default = match kind {
                        "unspecified" | "null" => json!({"kind":kind}),
                        "literal" | "expression" => {
                            json!({"kind":kind,"value":self.transfer_value(&column_default_key(i, name, "value"))})
                        }
                        _ => {
                            return Err((
                                column_default_key(i, name, "kind"),
                                "无效的列默认值类型".into(),
                            ));
                        }
                    };
                    defaults.insert(name.to_owned(), default);
                }
                let file_format = get(&format!("file-format-{i}"));
                // JSON 字符串数组可无损表达包含逗号或空白的列名。
                let list = |key: &str| -> std::result::Result<Vec<String>, (String, String)> {
                    let text = self.transfer_value(&format!("file-{key}-{i}"));
                    if text.trim().is_empty() {
                        Ok(Vec::new())
                    } else {
                        serde_json::from_str(text).map_err(|_| {
                            (
                                format!("file-{key}-{i}"),
                                "CSV 列列表必须是 JSON 字符串数组".into(),
                            )
                        })
                    }
                };
                let csv = datapier_core::model::transfer::CsvOptions {
                    encoding: get(&format!("file-encoding-{i}")),
                    delimiter: self
                        .transfer_value(&format!("file-delimiter-{i}"))
                        .to_owned(),
                    header: get(&format!("file-header-{i}")) == "true",
                    columns: list("columns")?,
                    null_marker: self
                        .transfer_value(&format!("file-null-marker-{i}"))
                        .to_owned(),
                    backslash_escape: get(&format!("file-backslash-{i}")) == "true",
                    binary_columns: list("binary-columns")?,
                };
                if mode == "import" && file_format == "csv" {
                    csv.validate().map_err(|error| {
                        let key = if error.message.contains("分隔符") {
                            "delimiter"
                        } else if error.message.contains("编码") {
                            "encoding"
                        } else {
                            "columns"
                        };
                        (format!("file-{key}-{i}"), error.message)
                    })?;
                }
                inputs.push(json!({"path":path,"format":if mode=="sql_restore" {"sql"} else {&file_format},"target":target,"column_mapping":mappings,"columns":types,"column_defaults":defaults,"record_ranges":ranges,"sync_schema":get(&format!("file-sync-{i}"))=="true","csv":csv}));
            }
        }
        let output = if matches!(mode.as_str(), "export" | "sql_dump") {
            if get("output").is_empty() {
                return Err(("output".into(), "输出文件路径不能为空".into()));
            }
            Some(json!({"path":get("output"),"format":if mode=="sql_dump" {"sql"} else {&format}}))
        } else {
            None
        };
        let mut objects = Vec::new();
        for i in 0..self.workbench.transfer.objects {
            let source_name = get(&format!("object-source-{i}"));
            if source_name.is_empty() {
                continue;
            }
            let target_name = get(&format!("object-target-{i}"));
            let source_db = match get(&format!("object-source-db-{i}")) {
                db if db.is_empty() && mode != "sql_restore" => get("source-database"),
                db => db,
            };
            let target_db = match get(&format!("object-target-db-{i}")) {
                db if db.is_empty() => get("destination-database"),
                db => db,
            };
            if self.workbench.transfer.project_mode
                && (!source_db.is_empty() || !target_db.is_empty())
            {
                return Err((
                    format!(
                        "object-{}-db-{i}",
                        if source_db.is_empty() {
                            "target"
                        } else {
                            "source"
                        }
                    ),
                    "项目迁移的实际数据库请通过本机绑定填写".into(),
                ));
            }
            let kind = get(&format!("object-kind-{i}"));
            let mut mappings = parse_mapping(&get(&format!("object-mapping-{i}")))
                .map_err(|e| (format!("object-mapping-{i}"), e))?;
            let explicitly_selected = !mappings.is_empty();
            mappings.retain(|_, target| target != "\0");
            if explicitly_selected && mappings.is_empty() {
                return Err((
                    format!("object-mapping-{i}"),
                    "请至少选择一个来源字段".into(),
                ));
            }
            if mappings
                .values()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != mappings.len()
            {
                return Err((format!("object-mapping-{i}"), "目标列为空或重复".into()));
            }
            let columns: Vec<_> = mappings
                .into_iter()
                .map(|(source, target)| json!({"source":source,"target":target}))
                .collect();
            let types = parse_types(&get(&format!("object-types-{i}")))
                .map_err(|e| (format!("object-types-{i}"), e))?;
            let mut filters = Vec::new();
            for j in 0..self
                .workbench
                .transfer
                .object_filters
                .get(i)
                .copied()
                .unwrap_or(1)
            {
                let column = get(&format!("object-filter-column-{i}-{j}"));
                if column.is_empty() {
                    continue;
                }
                let operator = get(&format!("object-filter-op-{i}-{j}"));
                let value_type = get(&format!("object-filter-type-{i}-{j}"));
                let text = self.transfer_value(&format!("object-filter-value-{i}-{j}"));
                if !matches!(operator.as_str(), "is_null" | "is_not_null") {
                    let valid = match value_type.as_str() {
                        "integer" => text.parse::<i64>().is_ok(),
                        "unsigned" => text.parse::<u64>().is_ok(),
                        "decimal" => text.parse::<bigdecimal::BigDecimal>().is_ok(),
                        "float" => text.parse::<f64>().is_ok_and(f64::is_finite),
                        _ => true,
                    };
                    if !valid {
                        return Err((
                            format!("object-filter-value-{i}-{j}"),
                            "过滤值与选择的数值类型不匹配".into(),
                        ));
                    }
                }
                let value = if matches!(operator.as_str(), "is_null" | "is_not_null") {
                    Value::Null
                } else if value_type == "bool" {
                    json!({"type":"bool","value":text.trim().parse::<bool>().map_err(|_| (format!("object-filter-value-{i}-{j}"), "布尔过滤值须为 true 或 false".into()))?})
                } else if value_type == "json" {
                    let value: Value = serde_json::from_str(text).map_err(|_| {
                        (
                            format!("object-filter-value-{i}-{j}"),
                            "JSON 过滤值无效".into(),
                        )
                    })?;
                    json!({"type":"json","value":value})
                } else if value_type == "bytes" {
                    use base64::{Engine as _, engine::general_purpose::STANDARD};
                    STANDARD.decode(text).map_err(|_| {
                        (format!("object-filter-value-{i}-{j}"), "Base64 无效".into())
                    })?;
                    json!({"type":"bytes","value":text})
                } else if value_type == "null" {
                    json!({"type":"null"})
                } else {
                    json!({"type":value_type,"value":text})
                };
                filters.push(json!({"column":column,"operator":operator,"value":value}));
            }
            objects.push(json!({"source":{"database":source_db,"name":source_name,"kind":kind,"key_b64":null},"target":{"database":target_db,"name":if target_name.is_empty(){source_name.clone()}else{target_name},"kind":kind,"key_b64":null},"columns":columns,"target_columns":types,"filters":filters,"create_if_missing":get(&format!("object-create-{i}"))=="true","sync_schema":get(&format!("object-sync-{i}"))=="true"}));
        }
        // 空 Base64 是合法的空字节 key；选择模式决定空文本的含义。
        let keys: Vec<_> = if get("redis-selection") == "keys" {
            let keys: Vec<_> = self
                .workbench
                .transfer
                .redis_keys
                .as_ref()
                .map(|keys| keys.iter().cloned().collect())
                .unwrap_or_else(|| {
                    let text = self.transfer_value("redis-keys");
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        text.split(',').map(|s| s.trim().to_owned()).collect()
                    }
                });
            if keys.is_empty() {
                return Err(("redis-keys".into(), "请至少选择一个 key".into()));
            }
            keys
        } else {
            Vec::new()
        };
        let prefix = get("source-prefix");
        let target_prefix = get("target-prefix");
        if prefix.is_empty() != target_prefix.is_empty() {
            return Err((
                if prefix.is_empty() {
                    "source-prefix"
                } else {
                    "target-prefix"
                }
                .into(),
                "源前缀和目标前缀必须同时指定".into(),
            ));
        }
        if provider == "redis" {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            for (field, values) in [
                (
                    "redis-keys",
                    keys.iter().map(String::as_str).collect::<Vec<_>>(),
                ),
                ("source-prefix", vec![prefix.as_str()]),
                ("target-prefix", vec![target_prefix.as_str()]),
            ] {
                if values
                    .into_iter()
                    .any(|value| STANDARD.decode(value).is_err())
                {
                    return Err((field.into(), "Base64 无效".into()));
                }
            }
        }
        let definer_mode = get("definer-mode");
        let definer = if definer_mode == "map" {
            json!({"mode":"map","accounts":parse_mapping(&get("definer-map")).map_err(|e| ("definer-map".into(), e))?})
        } else {
            json!({"mode":definer_mode})
        };
        let mut spec = json!({"mode":mode,"provider":provider,"source":source,"destination":destination,"inputs":inputs,"output":output,"objects":objects,"redis":{"keys_b64":keys,"pattern":if get("redis-selection")=="keys" || get("pattern").is_empty(){None}else{Some(get("pattern"))},"source_prefix_b64":if prefix.is_empty(){None}else{Some(prefix)},"target_prefix_b64":if target_prefix.is_empty(){None}else{Some(target_prefix)}},"options":{"conflict":get("conflict"),"conversion_errors":get("conversion"),"include_schema":get("include-schema")=="true","include_data":get("include-data")=="true","create_missing":get("create-missing")=="true","definer":definer,"existing_triggers_confirmed":get("triggers-confirmed")=="true"}});
        if let Some(base) = &self.workbench.transfer.loaded_spec {
            retain_unedited_spec(&mut spec, base);
        }
        Ok(spec)
    }
    pub(super) fn transfer_object_defaults(&mut self, i: usize) {
        for (key, value) in [
            ("source", ""),
            ("target", ""),
            ("source-db", ""),
            ("target-db", ""),
            ("mapping", ""),
            ("types", ""),
            ("kind", "table"),
            ("create", "false"),
            ("sync", "false"),
        ] {
            self.transfer_set(&format!("object-{key}-{i}"), value);
        }
        if self.workbench.transfer.object_filters.len() <= i {
            self.workbench.transfer.object_filters.resize(i + 1, 0);
        }
        self.workbench.transfer.object_filters[i] = 1;
        self.transfer_filter_defaults(i, 0);
    }
    pub(super) fn transfer_filter_defaults(&mut self, i: usize, j: usize) {
        for (key, value) in [
            ("column", ""),
            ("op", "eq"),
            ("type", "text"),
            ("value", ""),
        ] {
            self.transfer_set(&format!("object-filter-{key}-{i}-{j}"), value);
        }
    }
    pub(crate) fn transfer_load_preset(&mut self, preset: Value) {
        self.dirty = true;
        let spec = &preset["spec"];
        self.transfer_set(
            "source-alias",
            spec["source"]["connection_alias"].as_str().unwrap_or(""),
        );
        self.transfer_set(
            "destination-alias",
            spec["destination"]["connection_alias"]
                .as_str()
                .unwrap_or(""),
        );
        for key in ["mode", "provider"] {
            if let Some(value) = spec[key].as_str() {
                self.transfer_set(key, value);
            }
        }
        self.transfer_set(
            "definer-mode",
            spec["options"]["definer"]["mode"]
                .as_str()
                .unwrap_or("current_user"),
        );
        self.transfer_set(
            "definer-map",
            format_map(&spec["options"]["definer"]["accounts"]),
        );
        // 触发器确认针对当前目标及本次执行，不能从共享预设继承。
        self.transfer_set("triggers-confirmed", "false");
        for (key, path) in [
            ("source-profile", &spec["source"]["profile_id"]),
            ("destination-profile", &spec["destination"]["profile_id"]),
            ("source-database", &spec["source"]["database"]),
            ("destination-database", &spec["destination"]["database"]),
            ("format", &spec["output"]["format"]),
            ("output", &spec["output"]["path"]),
            ("pattern", &spec["redis"]["pattern"]),
            ("conflict", &spec["options"]["conflict"]),
            ("conversion", &spec["options"]["conversion_errors"]),
        ] {
            self.transfer_set(
                key,
                path.as_str().unwrap_or(match key {
                    "format" => "csv",
                    "conflict" => "insert",
                    "conversion" => "stop",
                    _ => "",
                }),
            );
        }
        for (key, value) in [
            ("source-redis-db", &spec["source"]["redis_db"]),
            ("destination-redis-db", &spec["destination"]["redis_db"]),
            ("include-schema", &spec["options"]["include_schema"]),
            ("include-data", &spec["options"]["include_data"]),
            ("create-missing", &spec["options"]["create_missing"]),
        ] {
            self.transfer_set(
                key,
                if value.is_null() {
                    match key {
                        "include-schema" | "include-data" => "true".into(),
                        "create-missing" => "false".into(),
                        _ => String::new(),
                    }
                } else {
                    value.to_string()
                },
            );
        }
        self.transfer_set(
            "redis-selection",
            if spec["redis"]["keys_b64"]
                .as_array()
                .is_some_and(|keys| !keys.is_empty())
            {
                "keys"
            } else {
                "pattern"
            },
        );
        self.transfer_set(
            "redis-keys",
            spec["redis"]["keys_b64"]
                .as_array()
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default(),
        );
        self.workbench.transfer.redis_keys = spec["redis"]["keys_b64"].as_array().map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        });
        self.transfer_set(
            "source-prefix",
            spec["redis"]["source_prefix_b64"].as_str().unwrap_or(""),
        );
        self.transfer_set(
            "target-prefix",
            spec["redis"]["target_prefix_b64"].as_str().unwrap_or(""),
        );
        let inputs = spec["inputs"].as_array().cloned().unwrap_or_default();
        self.workbench.transfer.files = inputs.len().max(1);
        for i in 0..self.workbench.transfer.files {
            let item = inputs.get(i).cloned().unwrap_or(Value::Null);
            self.transfer_set(&format!("file-{i}"), item["path"].as_str().unwrap_or(""));
            self.transfer_set(
                &format!("file-target-{i}"),
                item["target"]["name"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("file-mapping-{i}"),
                format_map(&item["column_mapping"]),
            );
            self.transfer_set(&format!("file-types-{i}"), format_types(&item["columns"]));
            if let Some(columns) = item["columns"].as_array() {
                for column in columns {
                    if let Some(name) = column["name"].as_str() {
                        let default = &item["column_defaults"][name];
                        self.transfer_set(
                            &column_default_key(i, name, "kind"),
                            default["kind"].as_str().unwrap_or(""),
                        );
                        self.transfer_set(
                            &column_default_key(i, name, "value"),
                            default["value"].as_str().unwrap_or(""),
                        );
                    }
                }
            }
            let range = item["record_ranges"]
                .as_array()
                .map(|ranges| {
                    ranges
                        .iter()
                        .map(|r| format!("{}-{}", r["first"], r["last"]))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            self.transfer_set(&format!("file-range-{i}"), range);
            self.transfer_set(
                &format!("file-format-{i}"),
                item["format"].as_str().unwrap_or("csv"),
            );
            self.transfer_set(
                &format!("file-encoding-{i}"),
                item["csv"]["encoding"].as_str().unwrap_or("utf-8"),
            );
            self.transfer_set(
                &format!("file-delimiter-{i}"),
                item["csv"]["delimiter"].as_str().unwrap_or(","),
            );
            for (key, value) in [
                (
                    "header",
                    item["csv"]["header"].as_bool().unwrap_or(true).to_string(),
                ),
                (
                    "backslash",
                    item["csv"]["backslash_escape"]
                        .as_bool()
                        .unwrap_or(true)
                        .to_string(),
                ),
                (
                    "null-marker",
                    item["csv"]["null_marker"]
                        .as_str()
                        .unwrap_or("\\N")
                        .to_owned(),
                ),
                (
                    "columns",
                    item["csv"]["columns"]
                        .as_array()
                        .map(|v| json!(v).to_string())
                        .unwrap_or_else(|| "[]".into()),
                ),
                (
                    "binary-columns",
                    item["csv"]["binary_columns"]
                        .as_array()
                        .map(|v| json!(v).to_string())
                        .unwrap_or_else(|| "[]".into()),
                ),
            ] {
                self.transfer_set(&format!("file-{key}-{i}"), value);
            }
            self.transfer_set(
                &format!("file-sync-{i}"),
                if item["sync_schema"] == true {
                    "true"
                } else {
                    "false"
                },
            );
        }
        let objects = spec["objects"].as_array().cloned().unwrap_or_default();
        self.workbench.transfer.objects = objects.len().max(1);
        for i in 0..self.workbench.transfer.objects {
            self.transfer_object_defaults(i);
            let object = objects.get(i).cloned().unwrap_or(Value::Null);
            self.transfer_set(
                &format!("object-create-{i}"),
                object["create_if_missing"]
                    .as_bool()
                    .unwrap_or(false)
                    .to_string(),
            );
            self.transfer_set(
                &format!("object-sync-{i}"),
                object["sync_schema"].as_bool().unwrap_or(false).to_string(),
            );
            self.transfer_set(
                &format!("object-source-{i}"),
                object["source"]["name"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-target-{i}"),
                object["target"]["name"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-source-db-{i}"),
                object["source"]["database"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-target-db-{i}"),
                object["target"]["database"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-kind-{i}"),
                object["source"]["kind"].as_str().unwrap_or("table"),
            );
            if let Some(filters) = object["filters"].as_array() {
                self.workbench.transfer.object_filters[i] = filters.len().max(1);
                for (j, filter) in filters.iter().enumerate() {
                    self.transfer_set(
                        &format!("object-filter-column-{i}-{j}"),
                        filter["column"].as_str().unwrap_or(""),
                    );
                    self.transfer_set(
                        &format!("object-filter-op-{i}-{j}"),
                        filter["operator"].as_str().unwrap_or("eq"),
                    );
                    self.transfer_set(
                        &format!("object-filter-type-{i}-{j}"),
                        filter["value"]["type"].as_str().unwrap_or("text"),
                    );
                    let value = &filter["value"]["value"];
                    let text = if filter["value"]["type"] == "json" {
                        value.to_string()
                    } else {
                        value.as_str().map(str::to_owned).unwrap_or_else(|| {
                            if value.is_null() {
                                String::new()
                            } else {
                                value.to_string()
                            }
                        })
                    };
                    self.transfer_set(&format!("object-filter-value-{i}-{j}"), text);
                }
            }
            let mapping: BTreeMap<String, String> = object["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some((
                        c["source"].as_str()?.to_owned(),
                        c["target"].as_str()?.to_owned(),
                    ))
                })
                .collect();
            let mapping = serde_json::to_string(&mapping).unwrap_or_default();
            self.transfer_set(&format!("object-mapping-{i}"), mapping);
            self.transfer_set(
                &format!("object-types-{i}"),
                format_types(&object["target_columns"]),
            );
        }
        self.transfer_set("preset-name", preset["name"].as_str().unwrap_or(""));
        self.workbench.transfer.invalidate();
        self.workbench.transfer.loaded_spec = Some(spec.clone());
        self.workbench.transfer.selected_preset = Some(preset);
    }
}
