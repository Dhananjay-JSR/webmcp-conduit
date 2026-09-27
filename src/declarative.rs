//! L0 — declarative WebMCP: `<form>` elements carrying `toolname` /
//! `tooldescription` attributes, compiled to tools with zero JavaScript.
//!
//! This is the only tier that is genuinely browser-free *and* execution-free:
//! discovery is an HTML parse, and invocation is an ordinary form submission.
//!
//! Caveat worth stating plainly: the spec's own schema-synthesis algorithm is
//! still `TODO` ("The exact algorithms ... are TBD"), Chromium ships "a loose
//! version", and ChatGPT does not support declarative tools at all. So this
//! tier is correct-by-construction but has very little to find in the wild
//! today. It is cheap to keep and it is where the standard is heading.

use crate::tool::{Annotations, FormAction, WebTool};
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Map, Value};

pub fn extract(html: &str, origin: &str) -> Vec<WebTool> {
    let doc = Html::parse_document(html);
    let form_sel = Selector::parse("form[toolname]").unwrap();

    doc.select(&form_sel)
        .filter_map(|form| tool_from_form(form, origin))
        .collect()
}

fn tool_from_form(form: ElementRef, origin: &str) -> Option<WebTool> {
    let el = form.value();
    let name = el.attr("toolname")?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let description = el
        .attr("tooldescription")
        .unwrap_or("Submit this form.")
        .trim()
        .to_string();

    let (schema, fixed) = synthesize_schema(form);

    Some(WebTool {
        name,
        title: None,
        description,
        input_schema: Some(schema),
        annotations: Annotations {
            // A form submission changes server state often enough that the
            // safe default is "not read-only". `method=get` is the exception.
            read_only_hint: el
                .attr("method")
                .map(|m| m.eq_ignore_ascii_case("get"))
                .unwrap_or(false),
            ..Default::default()
        },
        origin: origin.to_string(),
        form: Some(FormAction {
            action: el.attr("action").unwrap_or("").to_string(),
            method: el.attr("method").unwrap_or("get").to_uppercase(),
            enctype: el
                .attr("enctype")
                .unwrap_or("application/x-www-form-urlencoded")
                .to_string(),
            fixed,
            auto_submit: el.attr("toolautosubmit").is_some(),
        }),
    })
}

/// Compile a form's controls into a JSON Schema object, and collect the fields
/// that must be submitted but are not model-supplied (hidden inputs, presets).
fn synthesize_schema(form: ElementRef) -> (Value, Vec<(String, String)>) {
    let control_sel = Selector::parse("input, select, textarea").unwrap();

    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    let mut fixed: Vec<(String, String)> = Vec::new();
    // Radio groups share a name; collect their values into one enum.
    let mut radios: Map<String, Value> = Map::new();

    for ctl in form.select(&control_sel) {
        let el = ctl.value();
        let Some(field) = el.attr("name") else { continue };
        if field.is_empty() {
            continue;
        }
        let tag = el.name();
        let input_type = el.attr("type").unwrap_or("text").to_lowercase();

        if tag == "input" && input_type == "hidden" {
            fixed.push((field.to_string(), el.attr("value").unwrap_or("").to_string()));
            continue;
        }
        if tag == "input" && (input_type == "submit" || input_type == "button") {
            continue;
        }

        if tag == "input" && input_type == "radio" {
            let value = el.attr("value").unwrap_or("on").to_string();
            let entry = radios.entry(field.to_string()).or_insert_with(|| {
                json!({
                    "type": "string",
                    "enum": [],
                    "description": el.attr("toolparamdescription").unwrap_or("")
                })
            });
            if let Some(arr) = entry.get_mut("enum").and_then(|v| v.as_array_mut()) {
                arr.push(Value::String(value));
            }
            if el.attr("required").is_some() && !required.contains(&json!(field)) {
                required.push(json!(field));
            }
            continue;
        }

        let mut prop = match tag {
            "select" => select_schema(ctl),
            "textarea" => json!({"type": "string"}),
            _ => input_schema(&input_type, el),
        };

        if let Some(desc) = el.attr("toolparamdescription") {
            prop["description"] = json!(desc);
        }
        // Constraints that map cleanly onto JSON Schema validation.
        if let Some(v) = el.attr("minlength").and_then(|s| s.parse::<u64>().ok()) {
            prop["minLength"] = json!(v);
        }
        if let Some(v) = el.attr("maxlength").and_then(|s| s.parse::<u64>().ok()) {
            prop["maxLength"] = json!(v);
        }
        if let Some(p) = el.attr("pattern") {
            prop["pattern"] = json!(p);
        }
        if let Some(v) = el.attr("min").and_then(|s| s.parse::<f64>().ok()) {
            prop["minimum"] = json!(v);
        }
        if let Some(v) = el.attr("max").and_then(|s| s.parse::<f64>().ok()) {
            prop["maximum"] = json!(v);
        }

        if el.attr("required").is_some() {
            required.push(json!(field));
        }
        properties.insert(field.to_string(), prop);
    }

    for (k, v) in radios {
        properties.insert(k, v);
    }

    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));

    (Value::Object(schema), fixed)
}

fn input_schema(input_type: &str, el: &scraper::node::Element) -> Value {
    match input_type {
        "number" | "range" => {
            let mut v = json!({"type": "number"});
            // An integer step implies an integer field, which is a more useful
            // schema for a model than a bare number.
            if let Some(step) = el.attr("step") {
                if step.parse::<f64>().map(|s| s.fract() == 0.0).unwrap_or(false) {
                    v = json!({"type": "integer"});
                }
            }
            v
        }
        "checkbox" => json!({"type": "boolean"}),
        "email" => json!({"type": "string", "format": "email"}),
        "url" => json!({"type": "string", "format": "uri"}),
        "date" => json!({"type": "string", "format": "date"}),
        "time" => json!({"type": "string", "format": "time"}),
        "datetime-local" => json!({"type": "string", "format": "date-time"}),
        "tel" | "password" | "search" | "text" | _ => json!({"type": "string"}),
    }
}

fn select_schema(sel: ElementRef) -> Value {
    let opt_sel = Selector::parse("option").unwrap();
    let values: Vec<Value> = sel
        .select(&opt_sel)
        .map(|o| {
            let e = o.value();
            let v = e
                .attr("value")
                .map(|s| s.to_string())
                .unwrap_or_else(|| o.text().collect::<String>().trim().to_string());
            Value::String(v)
        })
        .collect();

    let multiple = sel.value().attr("multiple").is_some();
    if multiple {
        json!({"type": "array", "items": {"type": "string", "enum": values}})
    } else {
        json!({"type": "string", "enum": values})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"
    <html><body>
      <form toolname="search-cars"
            tooldescription="Perform a car make/model search"
            action="/search" method="get" toolautosubmit>
        <input type="text" name="make" toolparamdescription="The vehicle's make" required>
        <input type="text" name="model" toolparamdescription="The vehicle's model" required>
        <input type="number" name="year" min="1990" max="2030" step="1">
        <input type="hidden" name="csrf" value="abc123">
        <select name="fuel"><option value="petrol">Petrol</option><option value="ev">EV</option></select>
        <button type="submit">Search</button>
      </form>
      <form action="/newsletter"><input name="email"></form>
    </body></html>"#;

    #[test]
    fn only_annotated_forms_become_tools() {
        let tools = extract(PAGE, "https://cars.example");
        assert_eq!(tools.len(), 1, "the unannotated form must be ignored");
        assert_eq!(tools[0].name, "search-cars");
        assert_eq!(tools[0].description, "Perform a car make/model search");
    }

    #[test]
    fn schema_is_synthesized_from_controls() {
        let tools = extract(PAGE, "https://cars.example");
        let schema = tools[0].input_schema.as_ref().unwrap();
        let props = &schema["properties"];

        assert_eq!(props["make"]["type"], json!("string"));
        assert_eq!(props["make"]["description"], json!("The vehicle's make"));
        // step=1 means integer, not a bare number.
        assert_eq!(props["year"]["type"], json!("integer"));
        assert_eq!(props["year"]["minimum"], json!(1990.0));
        assert_eq!(props["fuel"]["enum"], json!(["petrol", "ev"]));

        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("make")));
        assert!(required.contains(&json!("model")));
        assert!(!required.contains(&json!("year")));
    }

    #[test]
    fn hidden_fields_are_carried_but_not_exposed_to_the_model() {
        let tools = extract(PAGE, "https://cars.example");
        let schema = tools[0].input_schema.as_ref().unwrap();
        assert!(
            schema["properties"].get("csrf").is_none(),
            "hidden inputs must not appear in the model-facing schema"
        );
        let form = tools[0].form.as_ref().unwrap();
        assert_eq!(form.fixed, vec![("csrf".to_string(), "abc123".to_string())]);
        assert!(form.auto_submit);
        assert_eq!(form.method, "GET");
    }

    #[test]
    fn get_forms_are_marked_read_only() {
        let tools = extract(PAGE, "https://cars.example");
        assert!(tools[0].annotations.read_only_hint);
    }
}
