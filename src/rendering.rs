use std::{fs, io};
use std::rc::Rc;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use handlebars::{Context, DirectorySourceOptions, Handlebars, Helper, HelperDef, HelperResult, JsonRender, JsonValue, Output, RenderContext, RenderError, RenderErrorReason, ScopedJson};
use qrcode::QrCode;
use qrcode::render;
use tokio::task::JoinSet;
use vb_exchange::{FilesOnMemoryOrHarddrive, NamedFile, RenderingError, RenderingRequest, RenderingResult, RenderingStatus};
use vb_exchange::export_formats::{ExportStepData, PandocExportStep, RawExportStep, VivliostyleExportStep, WeasyprintExportStep, WeasyprintPDFVariant};
use vb_exchange::projects::PreparedProject;
use crate::settings::Settings;
use crate::storage::Storage;
use html5ever::{local_name, ns, parse_document, parse_fragment, serialize, QualName};
use html5ever::tree_builder::{NodeOrText, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use markup5ever_rcdom::{Handle, NodeData, RcDom, SerializableHandle};
use svg::Document;
use svg::node::element::Text;
use log::{debug, error, warn};

pub async fn rendering_worker(storage: Arc<Storage>, settings: Arc<Settings>) {
    let subthreads_num = Arc::new(AtomicU64::new(0));

    loop{
        if subthreads_num.load(Ordering::SeqCst) >= settings.max_rendering_threads {
            println!("Too many running subthreads, waiting for one to end.");
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        let next_job = storage.request_queue.write().unwrap().pop_front();

        if let Some(job) = next_job{
            println!("Found RenderingRequest.");

            let render_request = Arc::new(job);
            let storage_cpy = Arc::clone(&storage);
            // Reserve the slot right away and release it whenever the task ends (success, failure or panic)
            let slot_guard = RenderingSlotGuard::acquire(Arc::clone(&subthreads_num));
            let timeout = Duration::from_secs(settings.rendering_timeout_secs);

            tokio::spawn(async move{
                let _slot_guard = slot_guard;

                // Get export formats to render
                let mut export_formats_queue = render_request.export_formats.clone();
                let request_status_storage = storage_cpy.request_status.clone();

                // Update status
                if let Some(status) = request_status_storage.write().unwrap().get_mut(&render_request.request_id){
                    *status = RenderingStatus::Running
                }

                let mut results: Vec<ExportFormatRenderingResult> = Vec::new();

                let mut join_set = JoinSet::new();

                while export_formats_queue.len() > 0{
                    let export_format_slug = export_formats_queue.pop().unwrap();
                    let render_request_cpy = Arc::clone(&render_request);
                    let storage_cpy2 = storage_cpy.clone();

                    join_set.spawn(tokio::task::spawn_blocking(move || {
                        match render_export_format(export_format_slug, Arc::clone(&storage_cpy2), Arc::clone(&render_request_cpy), timeout){
                            Ok(res) => {
                                Ok(res)
                            },
                            Err(e) => {
                                error!("Couldn't render export format: {:?}", e);
                                Err(e)
                            }
                        }
                    }));
                }

                while let Some(res) = join_set.join_next().await{
                    if let Ok(res) = res{
                        if let Ok(res) = res{
                            match res{
                                Ok(res) => {
                                    results.push(res)
                                }
                                Err(e) => {
                                    error!("Export Format failed rendering: {:?}", e);
                                    // Update status
                                    if let Some(status) = storage_cpy.request_status.write().unwrap().get_mut(&render_request.request_id){
                                        *status = RenderingStatus::Failed(e)
                                    }
                                    return;
                                }
                            }
                        }
                    }
                }

                let mut res_files : Vec<NamedFile> = vec![];
                // Load result files into memory, then delete files
                for res in results{
                    for file in &res.files_to_transfer{
                        let content = match tokio::fs::read(file).await {
                            Ok(data) => data,
                            Err(e) => {
                                error!("Failed to read the file: {}", e);
                                continue;
                            }
                        };
                        let filename = file.file_name().unwrap_or("invalid_filename".as_ref()).to_string_lossy().to_string();
                        res_files.push(NamedFile{ name: filename, content })
                    }
                    for dir in &res.temp_dirs{
                        if let Err(e) = tokio::fs::remove_dir_all(dir).await{
                            error!("Couldn't delete temp dir: {}. Keeping it for now.", e);
                        }
                    }
                }

                // Delete project uploads
                if let FilesOnMemoryOrHarddrive::Harddrive(path) = &render_request.project_uploaded_files{
                    if let Err(e) = tokio::fs::remove_dir_all(path).await{
                        error!("Couldn't delete project uploads dir: {}.", e);
                    }
                }

                // Update status
                if let Some(status) = request_status_storage.write().unwrap().get_mut(&render_request.request_id){
                    *status = RenderingStatus::Finished(RenderingResult{files: res_files})
                };
            });

        }else{
            println!("No Rendering Job in queue");
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Counts a running rendering job in the shared counter for as long as it lives
struct RenderingSlotGuard(Arc<AtomicU64>);

impl RenderingSlotGuard{
    fn acquire(counter: Arc<AtomicU64>) -> Self{
        counter.fetch_add(1, Ordering::SeqCst);
        RenderingSlotGuard(counter)
    }
}

impl Drop for RenderingSlotGuard{
    fn drop(&mut self){
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct ExportFormatRenderingResult{
    /// Paths to all files that should be transferred to main server
    files_to_transfer: Vec<PathBuf>,
    /// Paths to all created temp directories to delete after file transfer
    temp_dirs: Vec<PathBuf>,
}

pub fn render_export_format(slug: String, storage: Arc<Storage>, request: Arc<RenderingRequest>, timeout: Duration) -> Result<ExportFormatRenderingResult, RenderingError>{
    let mut rendering_log = String::new();

    let export_format = match storage.template_storage.read().unwrap().get(&request.template_id){
        Some(template) => {
            match template.export_formats.get(&slug){
                Some(ef) => ef.clone(),
                None => {
                    error!("Couldn't find export format {}.", slug);
                    return Err(RenderingError::TemplateNotFound)
                }
            }
        },
        None => {
            error!("Couldn't find template {} in storage.", &request.template_id);
            return Err(RenderingError::TemplateNotFound)
        }
    };

    rendering_log.push_str(&format!("Started rendering export format {}.", export_format.slug));

    let mut temp_directories : Vec<PathBuf> = Vec::new();
    let mut files_to_copy_into_next_export_steps: Vec<PathBuf> = Vec::new();

    for export_step in export_format.export_steps{
        rendering_log.push_str(&format!("Started rendering export step {}.", export_step.name));
        let files_to_keep = export_step.files_to_keep;

        // Prepare temp directory
        let temp_directory = match prepare_temp_directory(request.clone(), &export_format.slug){
            Ok(temp_id) => temp_id,
            Err(e) => {
                error!("Couldn't prepare temp directory: {}", e);
                return Err(RenderingError::Other("IO Error preparing temp directory.".to_string()));
            }
        };
        temp_directories.push(temp_directory.clone());
        rendering_log.push_str("Prepared temporary directory.");

        // Copy files from previous export step if any
        if files_to_copy_into_next_export_steps.len() > 0{
            for file_to_copy in &files_to_copy_into_next_export_steps {
                let filename = match file_to_copy.file_name(){
                    Some(filename) => filename.to_string_lossy().to_string(),
                    None => return Err(RenderingError::Other("Couldn't parse file name.".to_string()))
                };
                if let Err(e) = fs::copy(file_to_copy, temp_directory.join(filename.clone())){
                    rendering_log.push_str(&format!("Couldn't copy file to keep to new export step temp directory: {}", e.to_string()));
                    return Err(RenderingError::MissingExpectedFileToKeep(filename, rendering_log))
                }
            }

        }

        let res = match export_step.data{
            ExportStepData::Raw(raw) => render_raw_export_step(raw, &temp_directory, &request.prepared_project, &mut rendering_log),
            ExportStepData::Vivliostyle(vivlio) => render_vivliostyle_export_step(vivlio, &temp_directory, &mut rendering_log, timeout),
            ExportStepData::Pandoc(pan) => render_pandoc_export_step(pan, &temp_directory, &mut rendering_log, timeout),
            ExportStepData::Weasyprint(wes) => render_weasyprint_export_step(wes, &temp_directory, &mut rendering_log, timeout),
        };

        if let Err(e) = res{
            return Err(e);
        }

        for file in files_to_keep{
            let path = temp_directory.clone().join(PathBuf::from(file.clone()));
            if !path.exists(){
                return Err(RenderingError::MissingExpectedFileToKeep(file, rendering_log))
            }else{
                files_to_copy_into_next_export_steps.push(path);
            }
        }
    }

    let res = ExportFormatRenderingResult{
        files_to_transfer: files_to_copy_into_next_export_steps,
        temp_dirs: temp_directories,
    };

    Ok(res)
}

/// Prepares a new directory inside temp, copying all global_assets and assets of the given export format to this folder
///
/// Returns a PathBuf to the temp directory
fn prepare_temp_directory(request: Arc<RenderingRequest>, export_format_slug: &str) -> io::Result<PathBuf>{
    // Prepare temp dir:
    // Create new dir in temp/
    let random_id = uuid::Uuid::new_v4();
    let temp_dir_path = format!("temp/{}", &random_id);
    let temp_dir_path = Path::new(&temp_dir_path);
    fs::create_dir(temp_dir_path)?;

    let base_dir = format!("templates/{}", &request.template_version_id);
    let base_dir = Path::new(&base_dir);

    // Copy global assets
    copy_dir_all(base_dir.join("assets"), temp_dir_path.join("global_assets"))?;

    // Copy export format specific assets
    let dir_content = fs::read_dir(base_dir.join(format!("formats/{}", export_format_slug)))?;

    // Copy project uploads
    if let vb_exchange::FilesOnMemoryOrHarddrive::Harddrive(path) = &request.project_uploaded_files{
        copy_dir_all(path, temp_dir_path.join("uploads"))?;
    }

    for entry in dir_content{
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir(){
            copy_dir_all(entry.path(), temp_dir_path)?;
        }else{
            fs::copy(entry.path(), temp_dir_path.join(entry.file_name()))?;
        }
    }

    Ok(PathBuf::from(temp_dir_path))
}

/// Copies all contents from src dir to dst dir, creating the dst dir if necessary
fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

pub fn render_raw_export_step(step: RawExportStep, temp_dir: &PathBuf, prepared_project: &PreparedProject, rendering_log: &mut String) -> Result<(), RenderingError>{
    let mut handlebars = Handlebars::new();

    let mut dir_options = DirectorySourceOptions::default();
    dir_options.tpl_extension = String::from(".hbs.html");

    if let Err(e) = handlebars.register_templates_directory(temp_dir, dir_options){
        error!("Couldn't register templates: {}", e);
        return Err(RenderingError::CouldntLoadHandlebarTemplates(e.to_string()))
    }

    register_helpers(&mut handlebars);

    rendering_log.push_str("Starting handlebars rendering.");
    match handlebars.render(&step.entry_point.replace(".hbs.html", ""), prepared_project){
        Ok(res) => {
            if let Err(e) = fs::write(temp_dir.join(PathBuf::from(step.output_file)), res){
                error!("Couldn't write rendered template: {}", e);
                rendering_log.push_str(&format!("Couldn't write rendered template: {}", e));
                return Err(RenderingError::HandlebarsRenderingFailed(rendering_log.clone()))
            }
        },
        Err(e) => {
            error!("Handlebars rendering failed: {}", e);
            rendering_log.push_str(&format!("Handlebars rendering failed: {}", e));
            return Err(RenderingError::HandlebarsRenderingFailed(rendering_log.clone()));
        }
    }

    Ok(())
}

/// A number as the `contains` helper sees it: integers are kept exact, everything else is an f64.
enum LooseNumber {
    Int(i128),
    Float(f64),
}

/// True for plain decimal notation like `15`, `-3` or `1.50`. Signs like `+15`, leading zeros
/// (`007`) and exponents (`1e2`) are rejected so that string IDs don't turn into numbers.
fn is_plain_decimal(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    let int_ok = int_part == "0" || (!int_part.starts_with('0') && !int_part.is_empty() && int_part.bytes().all(|b| b.is_ascii_digit()));
    let frac_ok = frac_part.map_or(true, |f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()));
    int_ok && frac_ok
}

fn loose_number(v: &JsonValue) -> Option<LooseNumber> {
    match v {
        JsonValue::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Some(LooseNumber::Int(i as i128)),
            (_, Some(u)) => Some(LooseNumber::Int(u as i128)),
            _ => n.as_f64().map(LooseNumber::Float),
        },
        JsonValue::String(s) if is_plain_decimal(s) => match s.split_once('.') {
            // "15.0" is the integer 15, so it stays exact instead of going through f64
            Some((int_part, frac)) if frac.bytes().all(|b| b == b'0') => int_part.parse::<i128>().ok().map(LooseNumber::Int),
            Some(_) => s.parse::<f64>().ok().map(LooseNumber::Float),
            None => s.parse::<i128>().ok().map(LooseNumber::Int),
        },
        _ => None,
    }
}

fn loose_number_eq(a: &LooseNumber, b: &LooseNumber) -> bool {
    match (a, b) {
        (LooseNumber::Int(a), LooseNumber::Int(b)) => a == b,
        (LooseNumber::Float(a), LooseNumber::Float(b)) => a.is_finite() && a == b,
        // Compare exactly instead of converting the integer to f64, which would lose precision above 2^53.
        (LooseNumber::Int(i), LooseNumber::Float(f)) | (LooseNumber::Float(f), LooseNumber::Int(i)) => {
            f.is_finite() && f.fract() == 0.0 && f.abs() < 1e38 && *f as i128 == *i
        }
    }
}

/// The needle of a `contains` call with its number and text forms computed once, not per array item.
struct ContainsNeedle<'a> {
    value: &'a JsonValue,
    number: Option<LooseNumber>,
    text: Option<&'a str>,
}

impl<'a> ContainsNeedle<'a> {
    fn new(value: &'a JsonValue) -> Self {
        ContainsNeedle { value, number: loose_number(value), text: scalar_text(value) }
    }
}

/// Text form of a string or boolean. Numbers have none because they are only compared numerically.
fn scalar_text(v: &JsonValue) -> Option<&str> {
    match v {
        JsonValue::String(s) => Some(s),
        JsonValue::Bool(b) => Some(if *b { "true" } else { "false" }),
        _ => None,
    }
}

/// Loose equality used by `contains`. Null never matches. Arrays and objects only match
/// structurally identical values. Two strings only match if they are identical ("1.10" != "1.1").
/// Otherwise scalars match if they are numerically equal ("15" == 15 == 15.0) or have the same
/// text ("true" == true).
fn loose_eq(item: &JsonValue, needle: &ContainsNeedle) -> bool {
    match (item, needle.value) {
        (JsonValue::Null, _) | (_, JsonValue::Null) => false,
        (JsonValue::Array(_) | JsonValue::Object(_), _) | (_, JsonValue::Array(_) | JsonValue::Object(_)) => item == needle.value,
        (JsonValue::String(a), JsonValue::String(b)) => a == b,
        _ => {
            // Skip parsing the item when the needle can't be a number anyway
            if let Some(needle_number) = &needle.number {
                if let Some(item_number) = loose_number(item) {
                    return loose_number_eq(&item_number, needle_number);
                }
            }
            matches!((scalar_text(item), needle.text), (Some(a), Some(b)) if a == b)
        }
    }
}

/// Value helper: `{{#if (contains array value)}}...{{else}}...{{/if}}`
/// Returns true if the value is contained in the array. Scalars are compared loosely, so "15"
/// equals 15, "true" equals true and 15.0 equals 15 (see `loose_eq`). A missing or non-array first
/// argument is never a match.
struct ContainsHelper;

impl HelperDef for ContainsHelper {
    fn call_inner<'reg: 'rc, 'rc>(&self, h: &Helper<'rc>, _: &'reg Handlebars<'reg>, _: &'rc Context, _: &mut RenderContext<'reg, 'rc>) -> Result<ScopedJson<'rc>, RenderError> {
        let array_param = h.param(0).ok_or(RenderErrorReason::ParamNotFoundForIndex("contains", 0))?;
        let needle_param = h.param(1).ok_or(RenderErrorReason::ParamNotFoundForIndex("contains", 1))?;

        let found = match array_param.value() {
            JsonValue::Array(items) => {
                let needle = ContainsNeedle::new(needle_param.value());
                items.iter().any(|item| loose_eq(item, &needle))
            }
            _ => false,
        };

        Ok(ScopedJson::Derived(JsonValue::Bool(found)))
    }
}

/// Registers all custom helpers available in templates.
fn register_helpers(handlebars: &mut Handlebars) {
    handlebars.register_helper("qrcode", Box::new(handlebars_qrcode_helper));
    handlebars.register_helper("initial_letter", Box::new(handlebars_initial_letter_helper));
    handlebars.register_helper("contains", Box::new(ContainsHelper));
    handlebars.register_helper("combine_title_subtitle", Box::new(CombineTitleSubtitleHelper));
    handlebars.register_helper("html_to_text", Box::new(HtmlToTextHelper));
}

/// Converts a helper parameter to a string. `null` becomes an empty string, numbers are printed.
/// Booleans, arrays and objects have no sensible text form, so they are rendered as an empty string
/// and logged instead of ending up as `true` or JSON in the output.
fn text_param(helper: &'static str, index: usize, value: &JsonValue) -> String {
    match value {
        JsonValue::Null => String::new(),
        JsonValue::String(s) => s.clone(),
        JsonValue::Number(_) => value.render(),
        JsonValue::Bool(_) | JsonValue::Array(_) | JsonValue::Object(_) => {
            warn!("{}: parameter {} must be a string or number, rendering it as empty", helper, index);
            String::new()
        }
    }
}

/// Value helper: `{{html_to_text html}}`
/// Strips all tags from an HTML string and returns the plain text with entities decoded
/// (`<p>A &amp; B</p>` -> `A & B`). The input is parsed like a browser parses the body of a page, so
/// the same rules apply: a literal `<` or `&` in a plain-text value has to be written as `&lt;` /
/// `&amp;` to survive. Block-level elements and line breaks separate words, runs of whitespace are
/// collapsed, the contents of non-text elements (`script`, `style`, `template`, `title`, form
/// controls, ...) are dropped, and characters that are not allowed in XML 1.0 are removed.
///
/// The result is plain text, so `{{...}}` escaping it once for HTML/XML output is correct
/// (`A & B` is rendered as `A &amp; B`); use `{{{...}}}` for non-markup contexts.
struct HtmlToTextHelper;

impl HelperDef for HtmlToTextHelper {
    fn call_inner<'reg: 'rc, 'rc>(&self, h: &Helper<'rc>, _: &'reg Handlebars<'reg>, _: &'rc Context, _: &mut RenderContext<'reg, 'rc>) -> Result<ScopedJson<'rc>, RenderError> {
        let param = h.param(0).ok_or(RenderErrorReason::ParamNotFoundForIndex("html_to_text", 0))?;
        let html = text_param("html_to_text", 0, param.value());
        Ok(ScopedJson::Derived(JsonValue::String(html_to_plain_text(&html))))
    }
}

/// Elements whose contents are not text of the document.
fn is_non_text_element(name: &QualName) -> bool {
    name.ns == ns!(html) && matches!(&*name.local,
        "script" | "style" | "template" | "noscript" | "title" | "textarea" | "iframe" | "noembed" | "noframes" | "select" | "datalist")
}

/// The "special" elements of the HTML parsing spec
/// (https://html.spec.whatwg.org/multipage/parsing.html#special), which are the block-level and
/// replaced elements. They separate words, so adjacent blocks don't run together. `wbr` is left out,
/// as it marks a possible line break inside a word, and `legend` is added, as it is rendered as a block.
fn is_special_element(name: &QualName) -> bool {
    name.ns == ns!(html) && matches!(&*name.local,
        "address" | "applet" | "area" | "article" | "aside" | "base" | "basefont" | "bgsound" | "blockquote" | "body"
        | "br" | "button" | "caption" | "center" | "col" | "colgroup" | "dd" | "details" | "dir" | "div" | "dl" | "dt" | "embed"
        | "fieldset" | "figcaption" | "figure" | "footer" | "form" | "frame" | "frameset" | "h1" | "h2" | "h3" | "h4" | "h5"
        | "h6" | "head" | "header" | "hgroup" | "hr" | "html" | "iframe" | "img" | "input" | "keygen" | "legend" | "li" | "link"
        | "listing" | "main" | "marquee" | "menu" | "meta" | "nav" | "noembed" | "noframes" | "noscript"
        | "object" | "ol" | "p" | "param" | "plaintext" | "pre" | "script" | "search" | "section" | "select" | "source" | "style"
        | "summary" | "table" | "tbody" | "td" | "template" | "textarea" | "tfoot" | "th" | "thead" | "title" | "tr" | "track"
        | "ul" | "xmp")
}

/// Characters allowed in XML 1.0 documents (tab, line feed and carriage return are the only
/// allowed control characters).
fn is_xml_char(c: char) -> bool {
    !matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')
}

/// Parses an HTML snippet the way the body of a page is parsed. The nodes end up as children of the
/// returned root element; the DOM has to be kept alive while the root is used, as dropping it
/// empties the tree.
fn parse_html_fragment(html: &str) -> (RcDom, Handle) {
    let context = QualName::new(None, ns!(html), local_name!("body"));
    let dom = parse_fragment(RcDom::default(), Default::default(), context, Vec::new(), false).one(html);
    let root = dom.document.children.borrow()[0].clone();
    (dom, root)
}

fn parent_of(node: &Handle) -> Option<Handle> {
    let weak = node.parent.take()?;
    let parent = weak.upgrade();
    node.parent.set(Some(weak));
    parent
}

/// Appends the text below `node` to `out`, with a space around special elements. Iterative, so
/// deeply nested markup cannot overflow the stack. `template` contents are not children of the
/// template element in the DOM and are skipped implicitly.
fn collect_text(node: &Handle, out: &mut String) {
    // `None` marks the end of a special element.
    let mut stack = vec![Some(node.clone())];
    while let Some(entry) = stack.pop() {
        let Some(node) = entry else {
            out.push(' ');
            continue;
        };
        match &node.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { name, .. } if is_non_text_element(name) => continue,
            NodeData::Element { name, .. } if is_special_element(name) => {
                out.push(' ');
                stack.push(None);
            }
            _ => {}
        }
        stack.extend(node.children.borrow().iter().rev().cloned().map(Some));
    }
}

/// Collapses runs of whitespace to a single space, trims the ends and removes characters that are
/// not allowed in XML.
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().filter(|&c| is_xml_char(c)));
    }
    out
}

fn html_to_plain_text(html: &str) -> String {
    if !html.contains(['<', '&']) {
        return normalize_text(html);
    }
    let (_dom, root) = parse_html_fragment(html);
    let mut text = String::new();
    collect_text(&root, &mut text);
    normalize_text(&text)
}

/// Finds the last text node below `root` that contains more than whitespace, skipping non-text
/// elements.
fn last_text_node(root: &Handle) -> Option<Handle> {
    // Children are pushed in document order, so they are visited last to first.
    let mut stack: Vec<Handle> = root.children.borrow().clone();
    while let Some(node) = stack.pop() {
        match &node.data {
            NodeData::Text { contents } if !contents.borrow().trim().is_empty() => return Some(node.clone()),
            NodeData::Element { name, .. } if is_non_text_element(name) => {}
            _ => stack.extend(node.children.borrow().iter().cloned()),
        }
    }
    None
}

/// Comments and whitespace-only text, which don't change where a subtitle belongs.
fn is_insignificant(node: &Handle) -> bool {
    match &node.data {
        NodeData::Text { contents } => contents.borrow().trim().is_empty(),
        NodeData::Comment { .. } => true,
        _ => false,
    }
}

/// The text nodes below `nodes` in document order, skipping non-text elements.
fn text_nodes(nodes: &[Handle]) -> Vec<Handle> {
    let mut texts = Vec::new();
    let mut stack: Vec<Handle> = nodes.iter().rev().cloned().collect();
    while let Some(node) = stack.pop() {
        match &node.data {
            NodeData::Text { .. } => texts.push(node),
            NodeData::Element { name, .. } if is_non_text_element(name) => {}
            _ => stack.extend(node.children.borrow().iter().rev().cloned()),
        }
    }
    texts
}

/// Whether a block-level element with content is among `nodes` or below them. Such a subtitle cannot
/// be placed inside the title's block without nesting blocks in it.
fn contains_block(nodes: &[Handle]) -> bool {
    let mut stack = nodes.to_vec();
    while let Some(node) = stack.pop() {
        if let NodeData::Element { name, .. } = &node.data {
            if is_special_element(name) && !node.children.borrow().is_empty() {
                return true;
            }
        }
        stack.extend(node.children.borrow().iter().cloned());
    }
    false
}

enum SubtitleContent {
    /// Inline markup that can be moved into the title.
    Nodes(Vec<Handle>),
    /// Plain text of a subtitle whose markup would not fit into the title's block.
    Text(String),
}

/// What goes after the separator. A single block wrapper is removed (`<p>Sub</p>` -> `Sub`) so the
/// subtitle can be placed inside the title's block; a subtitle with further block-level content is
/// reduced to its text, as it cannot be nested inside the title.
fn subtitle_content(root: &Handle) -> SubtitleContent {
    let mut nodes = root.children.borrow().clone();
    loop {
        let mut content = nodes.iter().filter(|n| !is_insignificant(n));
        let (Some(only), None) = (content.next(), content.next()) else { break };
        let unwrap = matches!(&only.data, NodeData::Element { name, .. } if is_special_element(name) && !is_non_text_element(name))
            && !only.children.borrow().is_empty();
        if !unwrap {
            break;
        }
        let children = only.children.borrow().clone();
        nodes = children;
    }
    if contains_block(&nodes) {
        let mut text = String::new();
        for node in &nodes {
            collect_text(node, &mut text);
        }
        return SubtitleContent::Text(normalize_text(&text));
    }

    // Leading and trailing whitespace is dropped, also inside inline wrappers (`<em> Sub </em>`).
    let texts = text_nodes(&nodes);
    for node in &texts {
        let NodeData::Text { contents } = &node.data else { unreachable!() };
        let trimmed = StrTendril::from(contents.borrow().trim_start());
        let done = !trimmed.is_empty();
        *contents.borrow_mut() = trimmed;
        if done {
            break;
        }
    }
    for node in texts.iter().rev() {
        let NodeData::Text { contents } = &node.data else { unreachable!() };
        let trimmed = StrTendril::from(contents.borrow().trim_end());
        let done = !trimmed.is_empty();
        *contents.borrow_mut() = trimmed;
        if done {
            break;
        }
    }
    SubtitleContent::Nodes(nodes)
}

/// Normalizes an HTML snippet the same way `combine_title_subtitle` serializes its result, so every
/// return path has the same escaping (`&` -> `&amp;`) and leaves out characters not allowed in XML.
fn normalize_html(html: &str) -> String {
    if !html.contains(['<', '&', '>']) {
        return html.chars().filter(|&c| is_xml_char(c)).collect();
    }
    let (_dom, root) = parse_html_fragment(html);
    serialize_children(&root)
}

fn serialize_children(root: &Handle) -> String {
    let mut buffer = Vec::new();
    let root: SerializableHandle = root.clone().into();
    serialize(&mut buffer, &root, Default::default()).expect("serializing to a Vec cannot fail");
    // `&nbsp;` is the only named entity the serializer writes and it is not defined in XML.
    String::from_utf8(buffer).expect("serializer writes UTF-8").replace("&nbsp;", "\u{a0}").chars().filter(|&c| is_xml_char(c)).collect()
}

/// Characters after which a subtitle follows without a colon ("Warum? Untertitel").
const TITLE_TERMINAL_PUNCTUATION: [char; 12] = ['?', '!', '.', ':', ';', ',', '\u{2026}', '\u{2013}', '\u{2014}', '-', '\u{ff1f}', '\u{ff01}'];
/// Closing quotes that may follow the terminal punctuation (`"Warum?"`). Brackets are not included:
/// in "Verfassung (3. Aufl.)" the period ends the bracket, not the title.
const TITLE_TRAILING_QUOTES: [char; 11] = ['"', '\'', '\u{bb}', '\u{ab}', '\u{201c}', '\u{201d}', '\u{201e}', '\u{2018}', '\u{2019}', '\u{203a}', '\u{2039}'];

/// Value helper: `{{combine_title_subtitle title subtitle}}`
/// Joins title and subtitle with a space if the title ends with terminal punctuation (`? ! . : ; ,
/// - – — …`, optionally followed by closing quotes), otherwise as "Title: Subtitle".
/// Markup in the title is ignored when looking at its last character and is kept in the output. The
/// subtitle is placed right after the title's last text, inside the innermost block element around
/// it (`<p>Titel</p>` -> `<p>Titel: Untertitel</p>`, `Das <em>GG</em>` -> `Das <em>GG</em>: Untertitel`).
/// A single block wrapper around the subtitle is removed; a subtitle with more block-level content
/// (several paragraphs, lists, tables) is inserted as plain text. A missing subtitle or one without
/// text yields just the title, a title without text just the subtitle.
///
/// The result is always re-serialized HTML (a plain `&` becomes `&amp;`, void elements are written
/// as `<br>`), so output it with `{{{...}}}` in HTML. It is not well-formed XML: for XML/XMP output
/// wrap it in `html_to_text` and use `{{...}}`.
struct CombineTitleSubtitleHelper;

impl HelperDef for CombineTitleSubtitleHelper {
    fn call_inner<'reg: 'rc, 'rc>(&self, h: &Helper<'rc>, _: &'reg Handlebars<'reg>, _: &'rc Context, _: &mut RenderContext<'reg, 'rc>) -> Result<ScopedJson<'rc>, RenderError> {
        let title_param = h.param(0).ok_or(RenderErrorReason::ParamNotFoundForIndex("combine_title_subtitle", 0))?;
        let title = text_param("combine_title_subtitle", 0, title_param.value());
        let title = title.trim();
        let subtitle = h.param(1).map(|p| text_param("combine_title_subtitle", 1, p.value())).unwrap_or_default();
        let subtitle = subtitle.trim();
        if subtitle.is_empty() {
            return Ok(ScopedJson::Derived(JsonValue::String(normalize_html(title))));
        }
        Ok(ScopedJson::Derived(JsonValue::String(combine_title_subtitle(title, subtitle))))
    }
}

fn combine_title_subtitle(title: &str, subtitle: &str) -> String {
    let (subtitle_dom, subtitle_root) = parse_html_fragment(subtitle);
    if last_text_node(&subtitle_root).is_none() {
        return normalize_html(title);
    }
    let (title_dom, title_root) = parse_html_fragment(title);
    let Some(last_text) = last_text_node(&title_root) else {
        return normalize_html(subtitle);
    };

    let mut title_text = String::new();
    collect_text(&title_root, &mut title_text);
    let ends_with_punctuation = title_text.trim_end().trim_end_matches(TITLE_TRAILING_QUOTES).ends_with(TITLE_TERMINAL_PUNCTUATION);
    let separator = if ends_with_punctuation { " " } else { ": " };

    if let NodeData::Text { contents } = &last_text.data {
        let trimmed = StrTendril::from(contents.borrow().trim_end());
        *contents.borrow_mut() = trimmed;
    }
    // Insert after the outermost inline ancestor that ends with the last text, inside the nearest
    // block. An inline element with more content after the text (`<span>Titel<br></span>`) is not
    // left, so the subtitle stays in front of that content.
    let mut anchor = last_text;
    let (parent, next_sibling) = loop {
        let parent = parent_of(&anchor).expect("the title root is an ancestor");
        let next_sibling = {
            let siblings = parent.children.borrow();
            let index = siblings.iter().position(|n| Rc::ptr_eq(n, &anchor)).expect("anchor is a child of its parent");
            let ends_parent = siblings[index + 1..].iter().all(is_insignificant);
            let inline = matches!(&parent.data, NodeData::Element { name, .. } if !is_special_element(name)) && !Rc::ptr_eq(&parent, &title_root);
            if ends_parent && inline {
                None
            } else {
                Some(siblings.get(index + 1).cloned())
            }
        };
        match next_sibling {
            Some(next_sibling) => break (parent, next_sibling),
            None => anchor = parent,
        }
    };
    let insert = |child: NodeOrText<Handle>| match &next_sibling {
        Some(sibling) => title_dom.append_before_sibling(sibling, child),
        None => title_dom.append(&parent, child),
    };

    insert(NodeOrText::AppendText(StrTendril::from(separator)));
    match subtitle_content(&subtitle_root) {
        SubtitleContent::Nodes(nodes) => {
            for node in nodes {
                subtitle_dom.remove_from_parent(&node);
                insert(NodeOrText::AppendNode(node));
            }
        }
        SubtitleContent::Text(text) => insert(NodeOrText::AppendText(StrTendril::from(text))),
    }
    serialize_children(&title_root)
}

fn handlebars_qrcode_helper(h: &Helper, _: &Handlebars, _: &Context, _rc: &mut RenderContext, out: &mut dyn Output) -> HelperResult{
    let param = h.param(0).ok_or(RenderErrorReason::ParamNotFoundForIndex("qrcode", 0))?;
    let qrcode_color_dark = if let Some(param) = h.param(1){
        param.value().render().to_string()
    }else{
        "#000000".to_string()
    };
    let qrcode_color_light = if let Some(param) = h.param(2){
        param.value().render().to_string()
    }else{
        "#ffffff".to_string()
    };
    let val : String = param.value().render();

    let qr_code = match QrCode::new(val.to_string()){
        Ok(qr_code) => qr_code,
        Err(e) => {
            error!("Couldn't create qr code: {}", e);
            return Err(RenderError::from(RenderErrorReason::Other(format!("Couldn't create qr code: {}", e))));
        }
    };

    let mut image = qr_code.render::<render::svg::Color>()
        .dark_color(render::svg::Color(&qrcode_color_dark))
        .light_color(render::svg::Color(&qrcode_color_light))
        .build();
    image = image.replace("<?xml version=\"1.0\" standalone=\"yes\"?>", "");

    out.write(&format!("<div class=\"qrcode\" alt=\"QR Code\" />{}</div>", image))?;
    Ok(())
}

fn handlebars_initial_letter_helper(h: &Helper, _: &Handlebars, _: &Context, _rc: &mut RenderContext, out: &mut dyn Output) -> HelperResult{
    let param = h.param(0).ok_or(RenderErrorReason::ParamNotFoundForIndex("initial_letter", 0))?;
    let param_str = param.value().render();
    let font_family_param = h.param(1).ok_or(RenderErrorReason::ParamNotFoundForIndex("initial_letter", 1))?;
    let font_family = font_family_param.value().render();
    
    let dom = parse_document(RcDom::default(), Default::default()).from_utf8().read_from(&mut param_str.as_bytes())?;

    let mut first_letter = String::new();
    initial_letter_find_first_text(&dom.document, & mut first_letter);

    let document: SerializableHandle = dom.document.clone().into();
    let mut buffer = Vec::new();
    serialize(& mut buffer, &document, Default::default()).expect("serialization failed");
    let serialized_html = String::from_utf8(buffer)?.replace("<html><head></head><body>", "").replace("</body></html>", "");
    if first_letter != "" {
        let first_letter_svg_node = Text::new(first_letter)
            .set("text-anchor", "middle")
            .set("dominant-baseline", "middle")
            .set("font-size", "100")
            .set("x", 55)
            .set("y", 55)
            .set("fill", "#000000")
            .set("font-family", format!("\"{}\"", font_family));
    
        let document = Document::new()
            .set("viewBox", (0, 0, 100, 100))
            .set("width", "1.1em")
            .set("height", "1.1em")
            .add(first_letter_svg_node);
    
        let svg_string = document.to_string();
        
        out.write(&format!("<div class=\"initial-letter\" style=\"float: left;\">{}</div>{}", svg_string, serialized_html))?;
    }else {
        out.write(&format!("{}", serialized_html))?;
    };
    Ok(())
}

fn initial_letter_find_first_text(node: &Handle, first_letter_str: & mut String) -> Option<Handle>{
    match node.data{
        NodeData::Text { ref contents } => {
            let parent = node.parent.take()?.upgrade()?;

            if check_initial_letter_class(&parent) {
                let mut text_ref = contents.borrow_mut();
                let tendril_string = text_ref.deref_mut();
                let mut text = tendril_string.to_string();
                if !text.is_empty() {
                    let mut text_iter = text.chars().into_iter();
                    let first_letter = text_iter.next()?.to_string();
                    text = text_iter.collect::<String>();
                    first_letter_str.push_str(&first_letter);
                }
                *tendril_string = StrTendril::from(text);
                return Some(node.clone());
            }else {
                return None;
            };
        },
        _ => {
            for child in node.children.borrow().iter(){
                if let Some(found) = initial_letter_find_first_text(child, first_letter_str) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn check_initial_letter_class(node: &Handle) -> bool{
    let mut has_initial_letter_class = false;

    debug!("{:?}", node.data);

    if let NodeData::Element { ref attrs, .. } = node.data{
        let attrs = attrs.borrow();
        for attr in attrs.iter(){
            if attr.value.to_string().as_str() == "initial-letter-true" {
                has_initial_letter_class = true;
            }
        };
    }
    has_initial_letter_class
}

struct CommandOutput{
    output: std::process::Output,
    /// True if the process exceeded the timeout and got killed
    timed_out: bool,
}

/// How long to wait for the output pipes to close after the process ended.
/// Protects against descendants that outlive the process and keep the pipes open.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Reads the pipe to its end in a separate thread. The buffer is shared so already read output
/// stays available even if the reader never finishes. The receiver gets a message once the pipe is closed.
fn spawn_pipe_reader<R: Read + Send + 'static>(mut pipe: R) -> (Arc<std::sync::Mutex<Vec<u8>>>, std::sync::mpsc::Receiver<()>){
    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    let buf_cpy = Arc::clone(&buf);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop{
            match pipe.read(&mut chunk){
                Ok(0) | Err(_) => break,
                Ok(n) => buf_cpy.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
        let _ = tx.send(());
    });
    (buf, rx)
}

/// Waits (bounded) for the reader to finish and returns everything it read so far
fn collect_pipe(reader: Option<(Arc<std::sync::Mutex<Vec<u8>>>, std::sync::mpsc::Receiver<()>)>, give_up_at: std::time::Instant) -> Vec<u8>{
    match reader{
        Some((buf, done)) => {
            if done.recv_timeout(give_up_at.saturating_duration_since(std::time::Instant::now())).is_err(){
                error!("Output pipe of process didn't close within {}s after the process ended, using partial output.", PIPE_DRAIN_GRACE.as_secs());
            }
            let data = buf.lock().unwrap().clone();
            data
        },
        None => Vec::new(),
    }
}

/// Runs the command and captures its output like [`Command::output`], but kills it if it runs longer than `timeout`.
fn run_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<CommandOutput>{
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child: Child = command.spawn()?;

    // Drain the pipes in separate threads so the child can't block on a full pipe buffer
    let stdout_reader = child.stdout.take().map(spawn_pipe_reader);
    let stderr_reader = child.stderr.take().map(spawn_pipe_reader);

    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop{
        match child.try_wait(){
            Ok(Some(status)) => break status,
            Ok(None) => {},
            Err(e) => {
                // Don't leave the process running unsupervised
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if std::time::Instant::now() >= deadline{
            timed_out = true;
            if let Err(e) = child.kill(){
                error!("Couldn't kill timed out process: {}", e);
            }
            match child.wait(){
                Ok(status) => break status,
                Err(e) => return Err(e),
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let give_up_at = std::time::Instant::now() + PIPE_DRAIN_GRACE;
    let stdout = collect_pipe(stdout_reader, give_up_at);
    let stderr = collect_pipe(stderr_reader, give_up_at);

    Ok(CommandOutput{output: std::process::Output{status, stdout, stderr}, timed_out})
}

/// Calls weasyprint via bubblewrap (for isolation) and renders the html to pdf
pub fn render_weasyprint_export_step(step: WeasyprintExportStep, temp_dir: &PathBuf, rendering_log: &mut String, timeout: Duration) -> Result<(), RenderingError>{
    // Start bubblewrap
    let mut command = Command::new("bwrap");
    command.arg("--die-with-parent");

    // Bubblewrap options
    command.arg("--unshare-all")
        .arg("--ro-bind").arg("/lib").arg("/lib")
        .arg("--ro-bind").arg("/lib64").arg("/lib64")
        .arg("--ro-bind").arg("/usr/lib").arg("/usr/lib")
        .arg("--ro-bind").arg("/usr/lib64").arg("/usr/lib64")
        .arg("--ro-bind").arg("/usr/bin/python3").arg("/usr/bin/python3")
        .arg("--tmpfs").arg("/tmp")
        .arg("--proc").arg("/proc")
        .arg("--dev").arg("/dev")
        .arg("--bind").arg(temp_dir).arg("/data")
        .arg("--ro-bind").arg("rendering-envs/weasyprint").arg("/env");

    if Path::new("/usr/share/fonts").exists(){
        command.arg("--ro-bind").arg("/usr/share/fonts").arg("/usr/share/fonts");
    }else {
        if Path::new("/usr/local/share/fonts").exists() {
            command.arg("--ro-bind").arg("/usr/local/share/fonts").arg("/usr/share/fonts/");
        }
    }

    if Path::new("/usr/share/color").exists(){
        command.arg("--ro-bind").arg("/usr/share/color").arg("/usr/share/color");
    }

    // Call weasyprint
    command.arg("/env/venv/bin/python3").arg("/env/venv/bin/weasyprint");

    // Add weasyprint options
    if let Some(variant) = step.pdf_variant{
        let variant_str = match variant{
            WeasyprintPDFVariant::PDF => {
                None
            }
            WeasyprintPDFVariant::PDFA1B => {
                Some("pdf/a-1b")
            }
            WeasyprintPDFVariant::PDFA2B => {
                Some("pdf/a-2b")
            }
            WeasyprintPDFVariant::PDFA3B => {
                Some("pdf/a-3b")
            }
            WeasyprintPDFVariant::PDFA4B => {
                Some("pdf/a-4b")
            }
            WeasyprintPDFVariant::PDFA2U => {
                Some("pdf/a-2u")
            }
            WeasyprintPDFVariant::PDFA3U => {
                Some("pdf/a-3u")
            }
            WeasyprintPDFVariant::PDFA4U => {
                Some("pdf/a-4u")
            }
            WeasyprintPDFVariant::PDFUA1 => {
                Some("pdf/ua-1")
            }
            WeasyprintPDFVariant::DEBUG => {
                Some("debug")
            }
        };

        if let Some(variant) = variant_str{
            command.arg("--pdf-variant").arg(variant);
        }
    }

    // Add optional XMP metadata files (paths relative to the data directory)
    for xmp_file in step.xmp_metadata_files.iter().flatten().map(|f| f.trim()).filter(|f| !f.is_empty()){
        command.arg("--xmp-metadata").arg(format!("/data/{}", xmp_file));
    }

    command.arg("--custom-metadata");

    // Add weasyprint input/output files
    command.arg(format!("/data/{}", step.input_file)).arg(format!("/data/{}", step.output_file));

    match run_with_timeout(&mut command, timeout) {
        Ok(CommandOutput{output: res1, timed_out}) => {
            let stdout = String::from_utf8(res1.stdout).unwrap_or("".to_string());
            let stderr = String::from_utf8(res1.stderr).unwrap_or("".to_string());
            if timed_out{
                rendering_log.push_str(&format!("Weasyprint got killed after exceeding the timeout of {}s. stdout: {:?}, stderr: {:?}", timeout.as_secs(), &stdout, &stderr));
                error!("Weasyprint got killed after exceeding the timeout of {}s.", timeout.as_secs());
                return Err(RenderingError::WeasyprintRenderingFailed(rendering_log.clone()))
            }
            let res = format!("Weasyprint ran. stdout: {:?}, stderr: {:?}", &stdout, &stderr);
            rendering_log.push_str(&res);
            debug!("Weasyprint ran. stdout: {:?}, stderr: {:?}", &stdout, &stderr);
            Ok(())
        },
        Err(e) => {
            rendering_log.push_str(&format!("Couldn't render with weasyprint: {}", e));
            Err(RenderingError::WeasyprintRenderingFailed(rendering_log.clone()))
        }
    }
}

pub fn render_vivliostyle_export_step(step: VivliostyleExportStep, temp_dir: &PathBuf, rendering_log: &mut String, timeout: Duration) -> Result<(), RenderingError>{
    // Start bubblewrap
    let mut command = Command::new("bwrap");
    command.arg("--die-with-parent");

    command.arg("--unshare-all").arg("--tmpfs").arg("/tmp").arg("--ro-bind").arg("/lib").arg("/lib").arg("--ro-bind").arg("/lib64").arg("/lib64").arg("--ro-bind").arg("/usr/lib").arg("/usr/lib").arg("--proc").arg("/proc").arg("--dev").arg("/dev");

    if Path::new("/usr/share/fonts").exists(){
        command.arg("--ro-bind").arg("/usr/share/fonts").arg("/usr/share/fonts");
    }else {
        if Path::new("/usr/local/share/fonts").exists() {
            command.arg("--ro-bind").arg("/usr/local/share/fonts").arg("/usr/share/fonts/");
        }
    }

    command.arg("--bind").arg(temp_dir).arg("/data").arg("--ro-bind").arg("rendering-envs/vivliostyle").arg("/env").arg("/env/node").arg("/env/node_modules/.bin/vivliostyle").arg("build").arg(format!("/data/{}", step.input_file));

    if step.press_ready{
        command.arg("-p");
    }

    command.arg("-o").arg(format!("/data/{}", step.output_file));
    command.arg("--executable-browser").arg("/env/chromium/chrome");
    command.arg("--host").arg("127.0.0.1");

    match run_with_timeout(&mut command, timeout) {
        Ok(CommandOutput{output: res, timed_out}) => {
            if timed_out{
                rendering_log.push_str(&format!("Vivliostyle got killed after exceeding the timeout of {}s. stdout: {:?}, stderr: {:?}", timeout.as_secs(), String::from_utf8(res.stdout), String::from_utf8(res.stderr)));
                error!("Vivliostyle got killed after exceeding the timeout of {}s.", timeout.as_secs());
                return Err(RenderingError::VivliostyleRenderingFailed(rendering_log.clone()))
            }
            let res = format!("Vivliostyle ran. stdout: {:?}, stderr: {:?}", String::from_utf8(res.stdout), String::from_utf8(res.stderr));
            rendering_log.push_str(&res);
            if !res.contains("Built successfully"){
                return Err(RenderingError::VivliostyleRenderingFailed(rendering_log.clone()))
            }
            Ok(())
        },
        Err(e) => {
            rendering_log.push_str(&format!("Couldn't run vivliostyle: {}", e));
            Err(RenderingError::VivliostyleRenderingFailed(rendering_log.clone()))
        }
    }
}

pub fn render_pandoc_export_step(step: PandocExportStep, temp_dir: &PathBuf, rendering_log: &mut String, timeout: Duration) -> Result<(), RenderingError>{
    debug!("Started rendering pandoc export step.");
    let mut command = Command::new("bwrap");
    command.arg("--die-with-parent");

    command.arg("--unshare-all").arg("--bind").arg(temp_dir).arg("/data").arg("--ro-bind").arg("rendering-envs/pandoc").arg("/env").arg("/env/pandoc");

    command.arg("-o").arg(format!("/data/{}", step.output_file)).arg("-t").arg(step.output_format.to_string());
    command.arg("-f").arg(step.input_format.to_string());

    if let Some(shift) = step.shift_heading_level_by{
        command.arg(format!("--shift-heading-level-by={}", shift));
    }
    if let Some(metadata_file) = step.metadata_file{
        command.arg(format!("--metadata-file={}", metadata_file));
    }
    if let Some(epub_cover_image_path) = step.epub_cover_image_path{
        command.arg(format!("--epub-cover-image={}", epub_cover_image_path));
    }
    if let Some(epub_title_page) = step.epub_title_page{
        if epub_title_page{
            command.arg("--epub-title-page=true");
        }else{
            command.arg("--epub-title-page=false");
        }
    }
    if let Some(epub_metadata_file) = step.epub_metadata_file{
        command.arg(format!("--epub-metadata={}", epub_metadata_file));
    }
    if let Some(epub_embed_fonts) = step.epub_embed_fonts{
        for font in epub_embed_fonts{
            command.arg(format!("--epub-embed-font={}", font));
        }
    }

    command.arg(format!("data/{}", step.input_file));

    match run_with_timeout(&mut command, timeout) {
        Ok(CommandOutput{output: res1, timed_out}) => {
            let stdout = String::from_utf8(res1.stdout).unwrap_or("".to_string());
            let stderr = String::from_utf8(res1.stderr).unwrap_or("".to_string());
            if timed_out{
                rendering_log.push_str(&format!("Pandoc got killed after exceeding the timeout of {}s. stdout: {:?}, stderr: {:?}", timeout.as_secs(), &stdout, &stderr));
                error!("Pandoc got killed after exceeding the timeout of {}s.", timeout.as_secs());
                return Err(RenderingError::PandocConversionFailed(rendering_log.clone()))
            }
            let res = format!("Pandoc ran. stdout: {:?}, stderr: {:?}", &stdout, &stderr);
            rendering_log.push_str(&res);
            Ok(())
        },
        Err(e) => {
            rendering_log.push_str(&format!("Couldn't start pandoc: {}", e));
            Err(RenderingError::PandocConversionFailed(rendering_log.clone()))
        }
    }
}
#[cfg(test)]
mod tests{
    use super::*;

    #[test]
    fn run_with_timeout_kills_hanging_process(){
        let start = std::time::Instant::now();
        let res = run_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(300)).unwrap();
        assert!(res.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn run_with_timeout_returns_output_of_finished_process(){
        let res = run_with_timeout(Command::new("echo").arg("hi"), Duration::from_secs(10)).unwrap();
        assert!(!res.timed_out);
        assert_eq!(res.output.stdout, b"hi\n");
    }

    #[test]
    fn run_with_timeout_does_not_hang_on_descendants_holding_pipes(){
        // The shell exits immediately, the background sleep keeps stdout open
        let start = std::time::Instant::now();
        let res = run_with_timeout(Command::new("sh").arg("-c").arg("echo hi; sleep 30 &"), Duration::from_secs(10)).unwrap();
        assert!(!res.timed_out);
        assert_eq!(res.output.stdout, b"hi\n");
        assert!(start.elapsed() < Duration::from_secs(15));
    }

    #[test]
    fn slot_guard_releases_on_drop(){
        let counter = Arc::new(AtomicU64::new(0));
        {
            let _guard = RenderingSlotGuard::acquire(Arc::clone(&counter));
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    /// Renders `{{contains <haystack> <needle>}}` for a JSON object `data` and returns the resulting bool.
    fn contains(data: &str, args: &str) -> bool {
        let mut hb = Handlebars::new();
        register_helpers(&mut hb);
        let data: JsonValue = data.parse().unwrap();
        match hb.render_template(&format!("{{{{contains {}}}}}", args), &data).unwrap().as_str() {
            "true" => true,
            "false" => false,
            other => panic!("unexpected output: {}", other),
        }
    }

    fn render(data: &str, tpl: &str) -> String {
        let mut hb = Handlebars::new();
        register_helpers(&mut hb);
        let data: JsonValue = data.parse().unwrap();
        hb.render_template(tpl, &data).unwrap()
    }

    #[test]
    fn contains_works_in_if_and_unless(){
        let data = r#"{"m": {"nums": [1, 15, 20], "strs": ["a", "b"]}}"#;
        assert_eq!(render(data, "{{#if (contains m.nums 15)}}yes{{else}}no{{/if}}"), "yes");
        assert_eq!(render(data, "{{#if (contains m.nums 16)}}yes{{else}}no{{/if}}"), "no");
        assert_eq!(render(data, "{{#if (contains m.strs \"b\")}}yes{{else}}no{{/if}}"), "yes");
        assert_eq!(render(data, "{{#unless (contains m.strs \"c\")}}no-match{{/unless}}"), "no-match");
    }

    #[test]
    fn contains_as_plain_expression_renders_bool(){
        assert_eq!(render(r#"{"nums": [15]}"#, "{{contains nums 15}}"), "true");
    }

    #[test]
    fn contains_compares_strings_numbers_and_bools_loosely(){
        let data = r#"{"nums": [1, 15], "mixed": ["true", "15", null]}"#;
        assert!(contains(data, "nums \"15\""));
        assert!(contains(data, "mixed 15"));
        assert!(contains(data, "mixed true"));
        assert!(!contains(data, "nums true"));
    }

    #[test]
    fn contains_compares_numbers_numerically(){
        let data = r#"{"floats": [1.5, 15.0], "ints": [15, 9007199254740993]}"#;
        assert!(contains(data, "floats 15"));
        assert!(contains(data, "floats \"1.50\""));
        assert!(contains(data, "ints 15.0"));
        assert!(!contains(data, "floats 2"));
        assert!(contains(data, "ints 9007199254740993"));
        assert!(!contains(data, "ints 9007199254740992"));
        assert!(!contains(data, "ints 9007199254740992.0"));
    }

    #[test]
    fn contains_keeps_whole_number_strings_exact(){
        let data = r#"{"ints": [15, 9007199254740993], "strs": ["15.0", "9007199254740993.00"]}"#;
        assert!(contains(data, "ints \"15.0\""));
        assert!(contains(data, "ints \"9007199254740993.0\""));
        assert!(!contains(data, "ints \"9007199254740992.0\""));
        assert!(contains(data, "strs 15"));
        assert!(contains(data, "strs 9007199254740993"));
        assert!(!contains(data, "strs 9007199254740992"));
    }

    #[test]
    fn contains_does_not_match_numbers_by_their_printed_form(){
        let data = r#"{"floats": [1e100], "strs": ["1e100"]}"#;
        assert!(!contains(data, "floats strs.[0]"));
        assert!(!contains(data, "strs floats.[0]"));
    }

    #[test]
    fn contains_compares_two_strings_as_text(){
        let data = r#"{"sections": ["1.10", "2", "007", "+15", "1e2"]}"#;
        assert!(contains(data, "sections \"1.10\""));
        assert!(!contains(data, "sections \"1.1\""));
        assert!(!contains(data, "sections \"2.0\""));
        assert!(contains(data, "sections \"007\""));
        // against a real number only plain decimal strings count as numbers
        assert!(contains(data, "sections 2"));
        assert!(!contains(data, "sections 7"));
        assert!(!contains(data, "sections 15"));
        assert!(!contains(data, "sections 100"));
    }

    #[test]
    fn contains_compares_objects_and_arrays_structurally(){
        let data = r#"{"authors": [{"name": "A"}, {"name": "B"}], "lists": [[1, 2]], "strs": ["[object]", "1,2"], "a": {"name": "B"}, "b": {"name": "C"}, "l": [1, 2]}"#;
        assert!(contains(data, "authors a"));
        assert!(!contains(data, "authors b"));
        assert!(!contains(data, "strs a"));
        assert!(contains(data, "lists l"));
        assert!(!contains(data, "strs l"));
    }

    #[test]
    fn contains_requires_both_arguments(){
        let mut hb = Handlebars::new();
        register_helpers(&mut hb);
        let data: JsonValue = r#"{"nums": [1]}"#.parse().unwrap();
        assert!(hb.render_template("{{contains nums}}", &data).is_err());
        assert!(hb.render_template("{{contains}}", &data).is_err());
    }

    #[test]
    fn contains_missing_or_non_array_is_false(){
        let data = r#"{"strs": ["a"], "title": "abc"}"#;
        assert!(!contains(data, "missing 1"));
        assert!(!contains(data, "strs missing"));
        assert!(!contains(data, "title \"a\""));
    }

    #[test]
    fn combine_title_subtitle_uses_colon_after_alphanumeric_title(){
        let data = r#"{"t": "Titel", "s": "Untertitel"}"#;
        assert_eq!(render(data, "{{combine_title_subtitle t s}}"), "Titel: Untertitel");
        assert_eq!(render(r#"{"t": "Band 2", "s": "Teil"}"#, "{{combine_title_subtitle t s}}"), "Band 2: Teil");
    }

    #[test]
    fn combine_title_subtitle_uses_space_after_special_character(){
        for title in ["Warum?", "Achtung!", "Titel.", "Titel:", "Titel –", "Titel,", "Titel…"] {
            let data = format!(r#"{{"t": "{}", "s": "Untertitel"}}"#, title);
            assert_eq!(render(&data, "{{combine_title_subtitle t s}}"), format!("{} Untertitel", title));
        }
    }

    #[test]
    fn combine_title_subtitle_uses_colon_after_brackets_and_quotes(){
        for title in ["Verfassung (Neuauflage)", "Das \\\"Grundgesetz\\\"", "C++", "100%"] {
            let data = format!(r#"{{"t": "{}", "s": "Kommentar"}}"#, title);
            assert_eq!(render(&data, "{{{combine_title_subtitle t s}}}"), format!("{}: Kommentar", title.replace("\\\"", "\"")));
        }
    }

    #[test]
    fn combine_title_subtitle_ignores_markup_when_checking_last_character(){
        let data = r#"{"t": "Das <em>Grundgesetz</em>", "s": "Kommentar"}"#;
        assert_eq!(render(data, "{{{combine_title_subtitle t s}}}"), "Das <em>Grundgesetz</em>: Kommentar");
        let data = r#"{"t": "Warum <em>nur</em>?", "s": "Kommentar"}"#;
        assert_eq!(render(data, "{{{combine_title_subtitle t s}}}"), "Warum <em>nur</em>? Kommentar");
    }

    #[test]
    fn combine_title_subtitle_inserts_subtitle_inside_block_wrapper(){
        let data = r#"{"t": "<p>Titel</p>", "s": "Untertitel"}"#;
        assert_eq!(render(data, "{{{combine_title_subtitle t s}}}"), "<p>Titel: Untertitel</p>");
        let data = r#"{"t": "<div><h1>Warum <em>nur</em>?</h1>\n</div>", "s": "Kommentar"}"#;
        assert_eq!(render(data, "{{{combine_title_subtitle t s}}}"), "<div><h1>Warum <em>nur</em>? Kommentar</h1>\n</div>");
        // inline markup stays around the title only
        let data = r#"{"t": "Das <em>Grundgesetz</em>", "s": "Kommentar"}"#;
        assert_eq!(render(data, "{{{combine_title_subtitle t s}}}"), "Das <em>Grundgesetz</em>: Kommentar");
    }

    #[test]
    fn combine_title_subtitle_treats_markup_only_title_as_empty(){
        assert_eq!(render(r#"{"t": "<p></p>", "s": "Untertitel"}"#, "{{{combine_title_subtitle t s}}}"), "Untertitel");
        assert_eq!(render(r#"{"t": "<p> <br> </p>", "s": "Untertitel"}"#, "{{{combine_title_subtitle t s}}}"), "Untertitel");
    }

    #[test]
    fn combine_title_subtitle_looks_through_closing_quotes(){
        for title in ["\\\"Warum?\\\"", "\u{201e}Warum?\u{201c}", "Was ist das\u{ff1f}","\u{bb}Warum?\u{ab}", "Das <em>Recht?</em>\u{201d}"] {
            let data = format!(r#"{{"t": "{}", "s": "Untertitel"}}"#, title);
            assert_eq!(render(&data, "{{{combine_title_subtitle t s}}}"), format!("{} Untertitel", title.replace("\\\"", "\"")));
        }
    }

    #[test]
    fn html_to_text_survives_deeply_nested_markup(){
        // the tree builder's scope checks make parsing quadratic in the nesting depth, so this checks
        // the walk is iterative at a depth that still parses quickly
        let html = "<div><b>".repeat(5_000) + "x";
        let data = JsonValue::from_iter([("h", JsonValue::String(html))]);
        let mut hb = Handlebars::new();
        register_helpers(&mut hb);
        assert_eq!(hb.render_template("{{html_to_text h}}", &data).unwrap(), "x");
    }

    #[test]
    fn html_to_text_drops_other_raw_text_and_form_elements(){
        let data = r#"{"h": "A<iframe><b>x</b></iframe>BC<noembed>n</noembed><select><option>o</option></select>D<datalist><option>d</option></datalist>E"}"#;
        assert_eq!(render(data, "{{html_to_text h}}"), "ABCDE");
    }

    #[test]
    fn html_to_text_drops_nested_template_content(){
        let data = r#"{"h": "A<template>x<template>y</template>z</template>B"}"#;
        assert_eq!(render(data, "{{html_to_text h}}"), "AB");
    }

    #[test]
    fn combine_title_subtitle_and_html_to_text_compose(){
        let data = r#"{"t": "Das <em>Grundgesetz</em>", "s": "Kommentar &amp; Materialien"}"#;
        assert_eq!(render(data, "{{{html_to_text (combine_title_subtitle t s)}}}"), "Das Grundgesetz: Kommentar & Materialien");
    }

    #[test]
    fn helpers_render_booleans_arrays_and_objects_as_empty(){
        let data = r#"{"a": ["x"], "o": {"k": 1}, "b": true, "t": "Titel"}"#;
        assert_eq!(render(data, "[{{html_to_text a}}]"), "[]");
        assert_eq!(render(data, "[{{html_to_text o}}]"), "[]");
        assert_eq!(render(data, "[{{html_to_text b}}]"), "[]");
        assert_eq!(render(data, "{{combine_title_subtitle a t}}"), "Titel");
        assert_eq!(render(data, "{{combine_title_subtitle t a}}"), "Titel");
        assert_eq!(render(data, "{{combine_title_subtitle t b}}"), "Titel");
    }

    #[test]
    fn combine_title_subtitle_does_not_look_into_brackets(){
        for title in ["Verfassung (3. Aufl.)", "Verfassung (Was nun?)", "Band [Teil 2.]"] {
            let data = format!(r#"{{"t": "{}", "s": "Kommentar"}}"#, title);
            assert_eq!(render(&data, "{{{combine_title_subtitle t s}}}"), format!("{}: Kommentar", title));
        }
    }

    #[test]
    fn combine_title_subtitle_places_subtitle_before_trailing_breaks_and_comments(){
        assert_eq!(render(r#"{"t": "Titel<br>", "s": "Sub"}"#, "{{{combine_title_subtitle t s}}}"), "Titel: Sub<br>");
        assert_eq!(render(r#"{"t": "<p>Titel <!-- x --></p>", "s": "Sub"}"#, "{{{combine_title_subtitle t s}}}"), "<p>Titel: Sub<!-- x --></p>");
        assert_eq!(render(r#"{"t": "<p>Titel<span></span></p>", "s": "Sub"}"#, "{{{combine_title_subtitle t s}}}"), "<p>Titel: Sub<span></span></p>");
    }

    #[test]
    fn combine_title_subtitle_treats_markup_only_subtitle_as_empty(){
        for subtitle in ["<p></p>", "<p><br></p>", "<!-- x -->", "<p> <em> </em> </p>"] {
            let data = format!(r#"{{"t": "<p>Titel</p>", "s": "{}"}}"#, subtitle);
            assert_eq!(render(&data, "{{{combine_title_subtitle t s}}}"), "<p>Titel</p>", "{}", subtitle);
        }
    }

    #[test]
    fn combine_title_subtitle_inserts_block_subtitles_as_text(){
        assert_eq!(render(r#"{"t": "<h1>Titel</h1>", "s": "<p>A</p><p>B &amp; C</p>"}"#, "{{{combine_title_subtitle t s}}}"), "<h1>Titel: A B &amp; C</h1>");
        assert_eq!(render(r#"{"t": "<p>T</p>", "s": "x<p>y</p>"}"#, "{{{combine_title_subtitle t s}}}"), "<p>T: x y</p>");
        assert_eq!(render(r#"{"t": "Titel", "s": "<table><tr><td>A</td><td>B</td></tr></table>"}"#, "{{{combine_title_subtitle t s}}}"), "Titel: A B");
        // inline markup and empty elements like <br> stay
        assert_eq!(render(r#"{"t": "<p>T</p>", "s": "<p><em>x</em><br>y</p>"}"#, "{{{combine_title_subtitle t s}}}"), "<p>T: <em>x</em><br>y</p>");
    }

    #[test]
    fn combine_title_subtitle_stays_before_content_after_the_last_text(){
        assert_eq!(render(r#"{"t": "<span>Titel<br></span>", "s": "Sub"}"#, "{{{combine_title_subtitle t s}}}"), "<span>Titel: Sub<br></span>");
        assert_eq!(render(r#"{"t": "<b><i>Titel</i> <!-- x --></b>", "s": "Sub"}"#, "{{{combine_title_subtitle t s}}}"), "<b><i>Titel</i> <!-- x --></b>: Sub");
    }

    #[test]
    fn combine_title_subtitle_trims_whitespace_inside_inline_wrappers(){
        assert_eq!(render(r#"{"t": "Titel", "s": "<em> Sub </em>"}"#, "{{{combine_title_subtitle t s}}}"), "Titel: <em>Sub</em>");
        assert_eq!(render(r#"{"t": "Titel", "s": "<b> </b> <em> Sub <i>x</i> </em> "}"#, "{{{combine_title_subtitle t s}}}"), "Titel: <b></b><em>Sub <i>x</i></em>");
    }

    #[test]
    fn combine_title_subtitle_unwraps_block_wrapped_subtitle(){
        assert_eq!(render(r#"{"t": "<p>Titel</p>", "s": "<p>Sub</p>"}"#, "{{{combine_title_subtitle t s}}}"), "<p>Titel: Sub</p>");
        assert_eq!(render(r#"{"t": "Titel", "s": "<div><p> Sub <em>x</em> </p></div>"}"#, "{{{combine_title_subtitle t s}}}"), "Titel: Sub <em>x</em>");
    }

    #[test]
    fn combine_title_subtitle_escapes_text_in_html_output(){
        let data = r#"{"t": "Tom & Jerry Show", "s": "a < b"}"#;
        assert_eq!(render(data, "{{{combine_title_subtitle t s}}}"), "Tom &amp; Jerry\u{a0}Show: a &lt; b");
        // html_to_text collapses the non-breaking space like any other whitespace
        assert_eq!(render(data, "{{html_to_text (combine_title_subtitle t s)}}"), "Tom &amp; Jerry Show: a &lt; b");
    }

    #[test]
    fn html_to_text_keeps_xmp_and_svg_title_text(){
        assert_eq!(render(r#"{"h": "A<xmp><i>y</i></xmp>B"}"#, "{{{html_to_text h}}}"), "A <i>y</i> B");
        assert_eq!(render(r#"{"h": "A<svg><title>T</title></svg>B"}"#, "{{html_to_text h}}"), "ATB");
    }

    #[test]
    fn html_to_text_follows_html_parsing_rules_for_select(){
        // a nested <select> closes the outer one, as in a browser
        assert_eq!(render(r#"{"h": "A<select><select>B"}"#, "{{html_to_text h}}"), "AB");
    }

    #[test]
    fn html_to_text_separates_spec_block_elements(){
        for (html, text) in [
            ("<details><summary>A</summary>B</details>C", "A B C"),
            ("<fieldset><legend>A</legend>B</fieldset>", "A B"),
            ("<hgroup><h1>A</h1></hgroup>B<center>C</center>", "A B C"),
            ("<menu><li>A</li><li>B</li></menu>", "A B"),
            ("Wort<wbr>trennung", "Worttrennung"),
        ] {
            let data = format!(r#"{{"h": "{}"}}"#, html);
            assert_eq!(render(&data, "{{html_to_text h}}"), text, "{}", html);
        }
    }

    #[test]
    fn html_to_text_removes_characters_not_allowed_in_xml(){
        assert_eq!(render(r#"{"h": "x\u0000y\u0001z"}"#, "{{html_to_text h}}"), "xyz");
        assert_eq!(render(r#"{"h": "x<b>\u0000</b>y"}"#, "{{html_to_text h}}"), "xy");
        assert_eq!(render(r#"{"h": "A&#x1;B&#xFFFE;C"}"#, "{{html_to_text h}}"), "ABC");
        assert_eq!(render(r#"{"t": "A&#x1;B", "s": "C"}"#, "{{{combine_title_subtitle t s}}}"), "AB: C");
    }

    #[test]
    fn combine_title_subtitle_escapes_the_same_without_subtitle_or_title(){
        let title = r#"{"t": "Tom & Jerry\u0001", "s": "Sub & Co\u0001"}"#;
        assert_eq!(render(title, "{{{combine_title_subtitle t}}}"), "Tom &amp; Jerry");
        assert_eq!(render(title, "{{{combine_title_subtitle t s}}}"), "Tom &amp; Jerry: Sub &amp; Co");
        assert_eq!(render(r#"{"t": "<p></p>", "s": "Sub & Co\u0001"}"#, "{{{combine_title_subtitle t s}}}"), "Sub &amp; Co");
        assert_eq!(render(r#"{"t": "Tom\u0001"}"#, "{{{combine_title_subtitle t}}}"), "Tom");
    }

    #[test]
    fn combine_title_subtitle_handles_missing_subtitle(){
        assert_eq!(render(r#"{"t": "Titel", "s": null}"#, "{{combine_title_subtitle t s}}"), "Titel");
        assert_eq!(render(r#"{"t": "Titel"}"#, "{{combine_title_subtitle t missing}}"), "Titel");
        assert_eq!(render(r#"{"t": "Titel", "s": "  "}"#, "{{combine_title_subtitle t s}}"), "Titel");
        assert_eq!(render(r#"{"t": "Titel"}"#, "{{combine_title_subtitle t}}"), "Titel");
    }

    #[test]
    fn combine_title_subtitle_requires_title_param(){
        let mut hb = Handlebars::new();
        register_helpers(&mut hb);
        assert!(hb.render_template("{{combine_title_subtitle}}", &JsonValue::Null).is_err());
    }

    #[test]
    fn html_to_text_strips_tags_and_decodes_entities(){
        let data = r#"{"h": "<p>Tom &amp; <em>Jerry</em> &lt;3 &quot;x&quot;</p>"}"#;
        assert_eq!(render(data, "{{{html_to_text h}}}"), "Tom & Jerry <3 \"x\"");
    }

    #[test]
    fn html_to_text_result_is_escaped_once_by_double_braces(){
        // the plain text "Titel & Co" is escaped exactly once for HTML/XML output
        let data = r#"{"h": "<b>Titel</b> &amp; Co"}"#;
        assert_eq!(render(data, "<title>{{html_to_text h}}</title>"), "<title>Titel &amp; Co</title>");
        assert_eq!(render(data, "{{{html_to_text h}}}"), "Titel & Co");
    }

    #[test]
    fn html_to_text_separates_all_block_elements(){
        for html in ["<section>A</section><section>B</section>", "<dl><dt>A</dt><dd>B</dd></dl>", "<figure>A<figcaption>B</figcaption></figure>", "<article>A</article><aside>B</aside>", "<header>A</header><footer>B</footer>"] {
            let data = format!(r#"{{"h": "{}"}}"#, html);
            assert_eq!(render(&data, "{{html_to_text h}}"), "A B", "{}", html);
        }
    }

    #[test]
    fn html_to_text_drops_noscript_title_and_textarea(){
        let data = r#"{"h": "A<noscript><p>fallback</p></noscript>B<title>T</title><textarea>X</textarea>C"}"#;
        assert_eq!(render(data, "{{html_to_text h}}"), "ABC");
    }

    #[test]
    fn html_to_text_keeps_plain_text_without_markup(){
        assert_eq!(render(r#"{"h": "  a   b\n c "}"#, "{{html_to_text h}}"), "a b c");
        assert_eq!(render(r#"{"h": "Q&amp;A &copy; 2020"}"#, "{{{html_to_text h}}}"), "Q&A © 2020");
        assert_eq!(render(r#"{"h": 42}"#, "{{html_to_text h}}"), "42");
    }

    #[test]
    fn html_to_text_separates_blocks_and_collapses_whitespace(){
        let data = r#"{"h": "<h1>Titel</h1><p>Zeile 1<br>Zeile 2</p>\n\n  <ul><li>a</li><li>b</li></ul>"}"#;
        assert_eq!(render(data, "{{html_to_text h}}"), "Titel Zeile 1 Zeile 2 a b");
        assert_eq!(render(r#"{"h": "fo<b>o</b>bar"}"#, "{{html_to_text h}}"), "foobar");
    }

    #[test]
    fn html_to_text_drops_script_and_style(){
        let data = r#"{"h": "<style>p{color:red}</style>A<script>alert(1)</script>B"}"#;
        assert_eq!(render(data, "{{html_to_text h}}"), "AB");
    }

    #[test]
    fn html_to_text_handles_plain_text_and_missing_values(){
        assert_eq!(render(r#"{"h": "Nur Text"}"#, "{{html_to_text h}}"), "Nur Text");
        assert_eq!(render(r#"{"h": null}"#, "{{html_to_text h}}"), "");
        assert_eq!(render(r#"{}"#, "{{html_to_text missing}}"), "");
        let mut hb = Handlebars::new();
        register_helpers(&mut hb);
        assert!(hb.render_template("{{html_to_text}}", &JsonValue::Null).is_err());
    }
}
