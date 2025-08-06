// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;
use crate::subcommands::node::common;
use crate::subcommands::node::subcommands::graph::args::GraphOrientation;

use anyhow::{format_err, Result};
use args::GraphNodeCommand;
use fidl_fuchsia_driver_development as fdd;
use itertools::Itertools;
use std::collections::{BTreeMap, HashMap};

pub struct ClusterInfo {
    pub koid: u64,
    pub driver_url: String,
}

const TAB: &str = "    ";

const DIGRAPH_PREFIX: &str = r#"digraph {
    forcelabels = true; splines="ortho"; ranksep = 5; nodesep = 1;
    node [ shape = "box" color = " #0a7965" penwidth = 2.25 fontname = "prompt medium" fontsize = 10 margin = 0.22 ];
    edge [ color = " #283238" penwidth = 1 style = solid fontname = "roboto mono" fontsize = 10 ];"#;

pub async fn graph_node(
    cmd: GraphNodeCommand,
    writer: &mut dyn std::io::Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    if cmd.generate_html {
        return generate_html(writer, cmd.svg_path);
    }

    let nodes = fuchsia_driver_dev::get_device_info(&driver_development_proxy, &[], false).await?;
    let nodes = common::filter_nodes(nodes, cmd.filter)?;
    let node_map = common::create_node_map(&nodes)?;

    writeln!(writer, "{}", DIGRAPH_PREFIX)?;
    match cmd.orientation {
        GraphOrientation::TopToBottom => writeln!(writer, r#"{TAB}rankdir = "TB""#).unwrap(),
        GraphOrientation::LeftToRight => writeln!(writer, r#"{TAB}rankdir = "LR""#).unwrap(),
    };

    let mut grouped_nodes: HashMap<u64, HashMap<String, Vec<&fdd::NodeInfo>>> = HashMap::new();
    let mut unbound_nodes: Vec<&fdd::NodeInfo> = vec![];
    for node in &nodes {
        let cluster_info = get_cluster_info(node, &node_map);
        match cluster_info {
            Some(info) => {
                grouped_nodes
                    .entry(info.koid)
                    .or_default()
                    .entry(info.driver_url)
                    .or_default()
                    .push(node);
            }
            None => {
                unbound_nodes.push(node);
            }
        };
    }

    let mut edge_ids = vec![];

    let mut service_edges: Vec<(&fdd::NodeInfo, &fdd::NodeInfo, Vec<String>)> = vec![];
    if cmd.services {
        for node in nodes.iter() {
            let offers_converted = node.offer_list.as_ref().map(|offers| {
                offers
                    .iter()
                    .filter_map(|offer| {
                        if let fidl_fuchsia_component_decl::Offer::Service(svc) = offer {
                            let Some(fidl_fuchsia_component_decl::Ref::Child(ref child_ref)) =
                                svc.source
                            else {
                                return None;
                            };

                            let source_moniker = &child_ref.name;
                            return nodes
                                .iter()
                                .find(|node| node.moniker.as_ref() == Some(source_moniker))
                                .map(|node| (svc.clone(), node));
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            });

            if let Some(offers) = offers_converted {
                for (offer_svc, source_node) in offers {
                    let edge_string = get_labeled_graph_edge(
                        &mut edge_ids,
                        source_node,
                        node,
                        format!(
                            "{}({})",
                            offer_svc.source_name.expect("name").as_str(),
                            offer_svc
                                .renamed_instances
                                .unwrap_or_default()
                                .iter()
                                .map(|e| format!(
                                    "{}-->{}",
                                    e.source_name.clone(),
                                    e.target_name.clone()
                                ))
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                        .as_str(),
                    )?;

                    service_edges.push((source_node, node, edge_string));
                }
            }
        }
    }

    for (koid, cluster_drivers) in grouped_nodes {
        writeln!(writer, "{TAB}subgraph \"cluster_{}\" {{", koid)?;
        writeln!(writer, "{TAB}{TAB}label = \"Host {}\";", koid)?;
        writeln!(writer, "{TAB}{TAB}style = \"filled,rounded\";")?;
        writeln!(writer, r#"{TAB}{TAB}fillcolor = " #b1b9be";"#)?;

        for (driver, cluster_nodes) in &cluster_drivers {
            // Get just the last bit of the url.
            let driver = driver.rsplit_once('/').unwrap_or(("", &driver)).1;
            writeln!(writer, "{TAB}{TAB}subgraph \"cluster_{}_{}\" {{", koid, driver)?;
            writeln!(writer, "{TAB}{TAB}{TAB}label = \"{}\";", driver)?;
            writeln!(writer, "{TAB}{TAB}{TAB}style = \"filled,rounded\";")?;
            writeln!(writer, r#"{TAB}{TAB}{TAB}fillcolor = " #dce0e3";"#)?;

            for node in cluster_nodes {
                print_graph_node(node, writer, format!("{TAB}{TAB}{TAB}").as_str())?;
            }

            service_edges.retain(|(service_edge_src, service_edge_target, edge_string)| {
                if cluster_nodes.iter().any(|n| n == service_edge_src)
                    && cluster_nodes.iter().any(|n| n == service_edge_target)
                {
                    for entry in edge_string {
                        writeln!(writer, "{TAB}{TAB}{}", entry).unwrap();
                    }

                    false
                } else {
                    true
                }
            });

            writeln!(writer, "{TAB}{TAB}}}")?;
        }

        service_edges.retain(|(service_edge_src, service_edge_target, edge_string)| {
            if cluster_drivers
                .iter()
                .any(|(_, cluster_nodes)| cluster_nodes.iter().any(|n| n == service_edge_src))
                && cluster_drivers.iter().any(|(_, cluster_nodes)| {
                    cluster_nodes.iter().any(|n| n == service_edge_target)
                })
            {
                for entry in edge_string {
                    writeln!(writer, "{TAB}{}", entry).unwrap();
                }
                false
            } else {
                true
            }
        });

        writeln!(writer, "{TAB}}}")?;
    }

    for unbound_node in unbound_nodes {
        print_graph_node(unbound_node, writer, format!("{TAB}").as_str())?;
    }

    for node in nodes.iter() {
        if let Some(child_ids) = &node.child_ids {
            for id in child_ids.iter().rev() {
                if let Some(child) = node_map.get(&id) {
                    print_graph_edge(&mut edge_ids, node, writer, child)?;
                }
            }
        }
    }

    for (_, _, edge_string) in &service_edges {
        for entry in edge_string {
            writeln!(writer, "{}", entry).unwrap();
        }
    }

    writeln!(writer, "}}")?;

    Ok(())
}

fn generate_html(writer: &mut dyn std::io::Write, svg: Option<String>) -> Result<()> {
    let mut svg_content = match svg {
        Some(svg) => std::fs::read_to_string(svg)?,
        None => std::io::read_to_string(std::io::stdin())?,
    };

    let re_edges = regex::Regex::new(r#"<g id="(\d+_\d+)" class=""#)?;
    let edge_caps = re_edges
        .captures_iter(svg_content.as_str())
        .map(|caps| caps.get(1).map(|the_match| the_match.as_str().to_string()).expect("match1"))
        .collect::<Vec<_>>();

    let re_services = regex::Regex::new(r#"<g id="(\d+)_(\d+)_(.*)_([xyz])" class=""#)?;
    let svc_caps = re_services
        .captures_iter(svg_content.as_str())
        .map(|caps| {
            (
                caps.get(1).map(|the_match| the_match.as_str().to_string()).expect("match1"),
                caps.get(2).map(|the_match| the_match.as_str().to_string()).expect("match2"),
                caps.get(3).map(|the_match| the_match.as_str().to_string()).expect("match3"),
                caps.get(4).map(|the_match| the_match.as_str().to_string()).expect("match4"),
            )
        })
        .collect::<Vec<_>>();

    let mut ids_map = HashMap::new();
    for (start, end, proto, suffix) in svc_caps {
        ids_map.insert(
            format!("\"{}_{}_{}_{}\"", start, end, proto, suffix),
            vec![
                format!("\"{}_{}_{}_x\"", start, end, proto), // intermediate node
                format!("\"{}_{}_{}_y\"", start, end, proto), // source to intermediate path
                format!("\"{}_{}_{}_z\"", start, end, proto), // intermediate to target path
            ],
        );
    }

    let i = svg_content.find("viewBox=").expect("viewbox");
    svg_content.insert_str(i, "id=\"svgImage\" ");

    writeln!(
        writer,
        r#"<!DOCTYPE html>
<html>
<style>
body {{
  overflow: hidden;
}}
</style>
<head>
    <title>Fuchsia Driver Node Graph</title>
</head>

<body>
    <h1>Node Graph</h1>
    <h2>Instructions:</h2>
    <p>
There are three layers of boxes represented. The driver host process, the driver component, and individual nodes. The graph edges represent parent-child relationships with the edges that end in boxes. The graph edges represent service routes with edges that end with arrows. You can hover over service edges to highlight them temporarily, or click them to select them to help when panning. The graph can be panned and zoomed with a mouse or trackpad.<br>
    </p>
    <span id="zoomValue">Zoom scale: 1</span>
    <div id="svgContainer">
    {}
    </div>
    <script>
        "#,
        svg_content
    )?;

    let simple_ids = edge_caps.iter().map(|id| format!("\"{}\"", id)).join(",\n    ");

    let ids = ids_map
        .iter()
        .map(|(k, v)| (k, v.join(",")))
        .map(|(k, v)| format!("[{}, [{}]]", k, v))
        .join(",\n    ");

    writeln!(
        writer,
        r#"
const svgImage = document.getElementById("svgImage");
const svgContainer = document.getElementById("svgContainer");

var viewBox = {{x:0,y:0,w:svgImage.clientWidth,h:svgImage.clientHeight}};
svgImage.setAttribute('viewBox', `${{viewBox.x}} ${{viewBox.y}} ${{viewBox.w}} ${{viewBox.h}}`);
const svgSize = {{w:svgImage.clientWidth,h:svgImage.clientHeight}};
var isPanning = false;
var startPoint = {{x:0,y:0}};
var endPoint = {{x:0,y:0}};;
var scale = 1;

function isTrackPad(e) {{
    var isTrackpad = false;
    if (e.wheelDeltaY) {{
        if (e.wheelDeltaY === (e.deltaY * -3)) {{
            isTrackpad = true;
        }}
    }}
    else if (e.deltaMode === 0) {{
        isTrackpad = true;
    }}

    return isTrackpad;
}}


svgContainer.onwheel = function(e) {{
    e.preventDefault();
    var w = viewBox.w;
    var h = viewBox.h;
    var mx = e.offsetX;//mouse x
    var my = e.offsetY;
    var dw = w*Math.sign(e.deltaY)*0.05;
    var dh = h*Math.sign(e.deltaY)*0.05;
    if (!isTrackPad(e)) {{
        dw = -dw;
        dh = -dh;
    }}
    var dx = dw*mx/svgSize.w;
    var dy = dh*my/svgSize.h;
    viewBox = {{x:viewBox.x+dx,y:viewBox.y+dy,w:viewBox.w-dw,h:viewBox.h-dh}};
    scale = svgSize.w/viewBox.w;
    zoomValue.innerText = `Zoom scale: ${{Math.round(scale*100)/100}}`;
    svgImage.setAttribute('viewBox', `${{viewBox.x}} ${{viewBox.y}} ${{viewBox.w}} ${{viewBox.h}}`);
}}


svgContainer.onmousedown = function(e){{
    isPanning = true;
    startPoint = {{x:e.x,y:e.y}};
}}

svgContainer.onmousemove = function(e){{
    if (isPanning) {{
        endPoint = {{x:e.x,y:e.y}};
        var dx = (startPoint.x - endPoint.x)/scale;
        var dy = (startPoint.y - endPoint.y)/scale;
        var movedViewBox = {{x:viewBox.x+dx,y:viewBox.y+dy,w:viewBox.w,h:viewBox.h}};
        svgImage.setAttribute('viewBox', `${{movedViewBox.x}} ${{movedViewBox.y}} ${{movedViewBox.w}} ${{movedViewBox.h}}`);
   }}
}}

svgContainer.onmouseup = function(e){{
    if (isPanning) {{
        endPoint = {{x:e.x,y:e.y}};
        var dx = (startPoint.x - endPoint.x)/scale;
        var dy = (startPoint.y - endPoint.y)/scale;
        viewBox = {{x:viewBox.x+dx,y:viewBox.y+dy,w:viewBox.w,h:viewBox.h}};
        svgImage.setAttribute('viewBox', `${{viewBox.x}} ${{viewBox.y}} ${{viewBox.w}} ${{viewBox.h}}`);
        isPanning = false;
   }}
}}

svgContainer.onmouseleave = function(e){{
 isPanning = false;
}}

// Define all colors. Indexes are:
// 0 = intermediate_node
// 1 = source_to_intermediate path
// 2 = intermediate_to_target path and arrowhead
// 3 = simple edges (parent-child)

const originalStrokes = ['#0a7965', '#566168', '#566168', '#283238'];
const originalFills = ['none', '#566168', '#566168', '#283238'];
const originalText = 'none';

const hoverStrokes = ['#faa500', '#faa500', '#faa500', '#faa500'];
const hoverFills = ['#faa500', '#faa500', '#faa500', '#faa500'];
const hoverText = 'none';

const highlightStrokes = ['#c7241f', '#c7241f', '#c7241f', '#c7241f'];
const highlightFills = ['#c7241f', '#c7241f', '#c7241f', '#c7241f'];
const highlightText = '#ffffff';

const simple_ids = [
    {simple_ids}
]

// Map the edge to all the elements that should be highlighted.
const ids = [
    {ids}
];

function normalizeColor(colorString) {{
  const ctx = document.createElement('canvas').getContext('2d');
  ctx.fillStyle = colorString;
  return ctx.fillStyle;
}}

simple_ids.forEach(id => {{
    const edgeTrigger = document.getElementById(id);

    edgeTrigger.addEventListener('click', function () {{
        let isHighlighted = normalizeColor(edgeTrigger.querySelector('path').style.stroke) == normalizeColor(highlightStrokes[3]);
        if (!isHighlighted) {{
            edgeTrigger.querySelectorAll('polygon').forEach(p => {{
                p.style.stroke = highlightStrokes[3];
                p.style.fill = highlightFills[3];
            }});

            edgeTrigger.querySelectorAll('polyline').forEach(p => {{
                p.style.stroke = highlightStrokes[3];
                p.style.fill = highlightFills[3];
            }});

            edgeTrigger.querySelector('path').style.stroke = highlightStrokes[3];
        }} else {{
             edgeTrigger.querySelectorAll('polygon').forEach(p => {{
                p.style.stroke = originalStrokes[3];
                p.style.fill = originalFills[3];
            }});

            edgeTrigger.querySelectorAll('polyline').forEach(p => {{
                p.style.stroke = originalStrokes[3];
                p.style.fill = originalFills[3];
            }});

            edgeTrigger.querySelector('path').style.stroke = originalStrokes[3];
        }}
    }});

    edgeTrigger.addEventListener('mouseenter', function () {{
        let isHighlighted = normalizeColor(edgeTrigger.querySelector('path').style.stroke) == normalizeColor(highlightStrokes[3]);
        if (isHighlighted) {{ return; }}

        edgeTrigger.querySelectorAll('polygon').forEach(p => {{
            p.style.stroke = hoverStrokes[3];
            p.style.fill = hoverFills[3];
        }});

        edgeTrigger.querySelectorAll('polyline').forEach(p => {{
            p.style.stroke = hoverStrokes[3];
            p.style.fill = hoverFills[3];
        }});

        edgeTrigger.querySelector('path').style.stroke = hoverStrokes[3];
    }});

    edgeTrigger.addEventListener('mouseleave', function () {{
        let isHighlighted = normalizeColor(edgeTrigger.querySelector('path').style.stroke) == normalizeColor(highlightStrokes[3]);
        if (isHighlighted) {{ return; }}

        edgeTrigger.querySelectorAll('polygon').forEach(p => {{
            p.style.stroke = originalStrokes[3];
            p.style.fill = originalFills[3];
        }});

        edgeTrigger.querySelectorAll('polyline').forEach(p => {{
            p.style.stroke = originalStrokes[3];
            p.style.fill = originalFills[3];
        }});

        edgeTrigger.querySelector('path').style.stroke = originalStrokes[3];
    }});
}});


ids.forEach(id => {{
    const edgeTrigger = document.getElementById(id[0]);

    edgeTrigger.addEventListener('click', function () {{
        let intermediate_node = id[1][0];
        let source_to_intermediate = id[1][1];
        let intermediate_to_target = id[1][2];

        let isHighlighted = normalizeColor(document.getElementById(intermediate_node).querySelector('ellipse').style.stroke) == normalizeColor(highlightStrokes[0]);
        if (!isHighlighted) {{
            document.getElementById(intermediate_node).querySelector('ellipse').style.stroke = highlightStrokes[0];
            document.getElementById(intermediate_node).querySelector('ellipse').style.fill = highlightFills[0];
            document.getElementById(intermediate_node).querySelector('text').style.stroke = highlightText;

            document.getElementById(source_to_intermediate).querySelector('polygon').style.stroke = highlightStrokes[1];
            document.getElementById(source_to_intermediate).querySelector('polygon').style.fill = highlightFills[1];
            document.getElementById(source_to_intermediate).querySelector('path').style.stroke = highlightStrokes[1];

            document.getElementById(intermediate_to_target).querySelector('polygon').style.stroke = highlightStrokes[2];
            document.getElementById(intermediate_to_target).querySelector('polygon').style.fill = highlightFills[2];
            document.getElementById(intermediate_to_target).querySelector('path').style.stroke = highlightStrokes[2];
        }} else {{
            document.getElementById(intermediate_node).querySelector('ellipse').style.stroke = originalStrokes[0];
            document.getElementById(intermediate_node).querySelector('ellipse').style.fill = originalFills[0];
            document.getElementById(intermediate_node).querySelector('text').style.stroke = originalText;

            document.getElementById(source_to_intermediate).querySelector('polygon').style.stroke = originalStrokes[1];
            document.getElementById(source_to_intermediate).querySelector('polygon').style.fill = originalFills[1];
            document.getElementById(source_to_intermediate).querySelector('path').style.stroke = originalStrokes[1];

            document.getElementById(intermediate_to_target).querySelector('polygon').style.stroke = originalStrokes[2];
            document.getElementById(intermediate_to_target).querySelector('polygon').style.fill = originalFills[2];
            document.getElementById(intermediate_to_target).querySelector('path').style.stroke = originalStrokes[2];
        }}
    }});

    edgeTrigger.addEventListener('mouseenter', function () {{
        let intermediate_node = id[1][0];
        let source_to_intermediate = id[1][1];
        let intermediate_to_target = id[1][2];

        let isHighlighted = normalizeColor(document.getElementById(intermediate_node).querySelector('ellipse').style.stroke) == normalizeColor(highlightStrokes[0]);
        if (isHighlighted) {{ return; }}

        document.getElementById(intermediate_node).querySelector('ellipse').style.stroke = hoverStrokes[0];
        document.getElementById(intermediate_node).querySelector('ellipse').style.fill = hoverFills[0];
        document.getElementById(intermediate_node).querySelector('text').style.stroke = hoverText;

        document.getElementById(source_to_intermediate).querySelector('polygon').style.stroke = hoverStrokes[1];
        document.getElementById(source_to_intermediate).querySelector('polygon').style.fill = hoverFills[1];
        document.getElementById(source_to_intermediate).querySelector('path').style.stroke = hoverStrokes[1];

        document.getElementById(intermediate_to_target).querySelector('polygon').style.stroke = hoverStrokes[2];
        document.getElementById(intermediate_to_target).querySelector('polygon').style.fill = hoverFills[2];
        document.getElementById(intermediate_to_target).querySelector('path').style.stroke = hoverStrokes[2];
    }});

    edgeTrigger.addEventListener('mouseleave', function () {{
        let intermediate_node = id[1][0];
        let source_to_intermediate = id[1][1];
        let intermediate_to_target = id[1][2];

        let isHighlighted = normalizeColor(document.getElementById(intermediate_node).querySelector('ellipse').style.stroke) == normalizeColor(highlightStrokes[0]);
        if (isHighlighted) {{ return; }}

        document.getElementById(intermediate_node).querySelector('ellipse').style.stroke = originalStrokes[0];
        document.getElementById(intermediate_node).querySelector('ellipse').style.fill = originalFills[0];
        document.getElementById(intermediate_node).querySelector('text').style.stroke = originalText;

        document.getElementById(source_to_intermediate).querySelector('polygon').style.stroke = originalStrokes[1];
        document.getElementById(source_to_intermediate).querySelector('polygon').style.fill = originalFills[1];
        document.getElementById(source_to_intermediate).querySelector('path').style.stroke = originalStrokes[1];

        document.getElementById(intermediate_to_target).querySelector('polygon').style.stroke = originalStrokes[2];
        document.getElementById(intermediate_to_target).querySelector('polygon').style.fill = originalFills[2];
        document.getElementById(intermediate_to_target).querySelector('path').style.stroke = originalStrokes[2];
    }});
}});
    "#
    )?;

    writeln!(
        writer,
        r#"    </script>

</body>
</html>
"#
    )?;

    Ok(())
}

fn print_graph_node(
    node: &fdd::NodeInfo,
    writer: &mut dyn std::io::Write,
    prefix: &str,
) -> Result<()> {
    let moniker = node.moniker.as_ref().ok_or_else(|| format_err!("Node missing moniker"))?;
    let (_, name) = moniker.rsplit_once('.').unwrap_or(("", &moniker));
    let node_id = node.id.as_ref().ok_or_else(|| format_err!("Node missing id"))?;

    writeln!(writer, "{}\"{}\" [label=\"{}\", id = \"{}\"]", prefix, node_id, name, node_id)?;
    Ok(())
}

fn print_graph_edge(
    edge_ids: &mut Vec<String>,
    node: &fdd::NodeInfo,
    writer: &mut dyn std::io::Write,
    child: &fdd::NodeInfo,
) -> Result<()> {
    let start_id = node.id.as_ref().ok_or_else(|| format_err!("Node missing id"))?;
    let end_id = child.id.as_ref().ok_or_else(|| format_err!("Child node missing id"))?;

    edge_ids.push(format!("{}_{}", start_id, end_id));

    writeln!(
        writer,
        "{TAB}\"{}\" -> \"{}\" [arrowhead = box id = \"{}_{}\"]",
        start_id, end_id, start_id, end_id
    )?;
    Ok(())
}

fn get_labeled_graph_edge(
    edge_ids: &mut Vec<String>,
    node: &fdd::NodeInfo,
    child: &fdd::NodeInfo,
    label: &str,
) -> Result<Vec<String>> {
    let start_id = node.id.as_ref().ok_or_else(|| format_err!("Node missing id"))?;
    let end_id = child.id.as_ref().ok_or_else(|| format_err!("Child node missing id"))?;

    let sanitized_label: String =
        label.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    let intermediate_node_id = format!("intermediate_{}_{}_{}", start_id, end_id, sanitized_label);

    let id_group = format!("{}_{}_{}", start_id, end_id, sanitized_label);
    edge_ids.push(format!("{}_x", id_group));
    edge_ids.push(format!("{}_y", id_group));
    edge_ids.push(format!("{}_z", id_group));

    Ok(vec![
        format!(
            "{TAB}\"{}\" [shape=oval, style=\"dotted\", label=\"{}\", id = \"{}_x\"];",
            intermediate_node_id, label, id_group
        ),
        format!(
            "{TAB}\"{}\" -> \"{}\" [dir=back arrowtail = inv, color = \" #566168\" penwidth = 1 style = solid, id = \"{}_y\"];",
            start_id, intermediate_node_id, id_group
        ),
        format!(
            "{TAB}\"{}\" -> \"{}\" [arrowhead = normal color = \" #566168\" penwidth = 1 style = solid, id = \"{}_z\"]",
            intermediate_node_id, end_id, id_group
        ),
    ])
}

/// For a given node, traverse up the tree to find the driver host koid and the driver URL
/// that owns the node.
fn get_cluster_info(
    node: &fdd::NodeInfo,
    node_map: &BTreeMap<u64, fdd::NodeInfo>,
) -> Option<ClusterInfo> {
    let mut koid = node.driver_host_koid;
    let (_, mut url) = common::get_state_and_owner(node.quarantined, &node.bound_driver_url, false);

    if url == "none" {
        return None;
    }

    let mut curr = node;
    while koid.is_none() || url == "parent" || url == "composite(s)" {
        let primary_parent_id = find_primary_parent(curr, node_map)?;
        curr = &node_map[primary_parent_id];

        let (_, owner) =
            common::get_state_and_owner(curr.quarantined, &curr.bound_driver_url, false);

        if koid.is_none() {
            koid = curr.driver_host_koid;
        }
        if url == "parent" || url == "composite(s)" {
            url = owner;
        }
    }

    Some(ClusterInfo { koid: koid.expect("koid should not be none"), driver_url: url })
}

/// Find the primary parent of a node. The primary parent is the parent that is a prefix of the
/// node's moniker.
fn find_primary_parent<'a>(
    node: &'a fdd::NodeInfo,
    node_map: &'a BTreeMap<u64, fdd::NodeInfo>,
) -> Option<&'a u64> {
    let moniker = node.moniker.as_ref()?;
    node.parent_ids.as_ref()?.iter().find(|parent_id: &&u64| {
        if let Some(parent_node) = node_map.get(parent_id) {
            if let Some(parent_moniker) = &parent_node.moniker {
                return moniker.starts_with(parent_moniker);
            }
        }
        false
    })
}
