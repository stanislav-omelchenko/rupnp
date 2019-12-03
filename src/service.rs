use crate::{
    error::{Error, UPnPError},
    find_in_xml,
    scpd::SCPD,
    utils::{self, HttpResponseExt},
    Result,
};

use async_std::{io::BufReader, net::TcpListener, prelude::*};
use genawaiter::sync::{Co, Gen};

use http::{uri::PathAndQuery, Uri};
use isahc::prelude::*;
use roxmltree::{Document, Node};
use ssdp_client::URN;

use std::collections::HashMap;

/// A UPnP Service is the description of endpoints on a device for performing actions and reading
/// the service definition.
/// For a list of actions and state variables the service provides, take a look at [`scpd`](struct.Service.html#method.scpd).
#[derive(Debug, Clone)]
pub struct Service {
    service_type: URN,
    service_id: String,
    scpd_endpoint: PathAndQuery,
    control_endpoint: PathAndQuery,
    event_sub_endpoint: PathAndQuery,
}

impl Service {
    pub(crate) fn from_xml(node: Node<'_, '_>) -> Result<Self> {
        #[allow(non_snake_case)]
        let (service_type, service_id, scpd_endpoint, control_endpoint, event_sub_endpoint) =
            find_in_xml! { node => serviceType, serviceId, SCPDURL, controlURL, eventSubURL };

        Ok(Self {
            service_type: utils::parse_node_text(service_type)?,
            service_id: utils::parse_node_text(service_id)?,
            scpd_endpoint: utils::parse_node_text(scpd_endpoint)?,
            control_endpoint: utils::parse_node_text(control_endpoint)?,
            event_sub_endpoint: utils::parse_node_text(event_sub_endpoint)?,
        })
    }

    /// Returns the [URN](ssdp_client::URN) of this service.
    pub fn service_type(&self) -> &URN {
        &self.service_type
    }

    /// Returns the `Service Identifier`.
    pub fn service_id(&self) -> &str {
        &self.service_id
    }

    pub(crate) fn control_url(&self, url: &Uri) -> Uri {
        replace_url_path(url, &self.control_endpoint)
    }
    pub(crate) fn scpd_url(&self, url: &Uri) -> Uri {
        replace_url_path(url, &self.scpd_endpoint)
    }
    pub(crate) fn event_sub_url(&self, url: &Uri) -> Uri {
        replace_url_path(url, &self.event_sub_endpoint)
    }

    /// Fetches the [`SCPD`](scpd/struct.SCPD.html) of this service.
    pub async fn scpd(&self, url: &Uri) -> Result<SCPD> {
        Ok(SCPD::from_url(&self.scpd_url(url), self.service_type().clone()).await?)
    }

    /// Execute some UPnP Action on this service.
    /// The URL is usually obtained by the device this service was found on.
    /// The payload is xml-formatted data.
    ///
    /// # Example usage:
    ///
    /// ```rust,no_run
    /// # use ssdp_client::URN;
    /// # async fn rendering_control_example() -> Result<(), upnp::Error> {
    /// # let some_url = unimplemented!();
    /// use upnp::ssdp::URN;
    /// use upnp::Device;
    ///
    /// let urn = URN::service("schemas-upnp-org", "RenderingControl", 1);
    ///
    /// let device = Device::from_url( some_url ).await?;
    /// let service = device.find_service(&urn)
    ///     .expect("service exists");
    ///
    /// let args = "<InstanceID>0</InstanceID><Channel>Master</Channel>";
    /// let response = service.action(device.url(), "GetVolume", args).await?;
    ///
    /// let volume = response
    ///     .get("CurrentVolume")
    ///     .expect("exists");
    ///
    /// println!("Volume: {}", volume);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn action(
        &self,
        url: &Uri,
        action: &str,
        payload: &str,
    ) -> Result<HashMap<String, String>> {
        let body = format!(
            r#"
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
                s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
                <s:Body>
                    <u:{action} xmlns:u="{service}">
                        {payload}
                    </u:{action}>
                </s:Body>
            </s:Envelope>"#,
            service = &self.service_type,
            action = action,
            payload = payload
        );

        let soap_action = format!("\"{}#{}\"", &self.service_type, action);

        let doc = Request::post(self.control_url(url))
            .header("CONTENT-TYPE", "xml")
            .header("SOAPAction", soap_action)
            .body(body)
            .unwrap()
            .send_async()
            .await?
            .err_if_not_200()?
            .text_async()
            .await?;

        let document = Document::parse(&doc)?;
        let body = utils::find_root(&document, "Body", "UPnP Response")?;

        let first_child = body.first_element_child().ok_or(Error::ParseError(
            "the upnp responses `Body` element has no children",
        ))?;

        if first_child.tag_name().name().eq_ignore_ascii_case("Fault") {
            Err(UPnPError::from_fault_node(first_child)?.into())
        } else if first_child.tag_name().name().starts_with(action) {
            Ok(first_child
                .children()
                .filter(Node::is_element)
                .filter_map(|node| -> Option<(String, String)> {
                    if let Some(text) = node.text() {
                        Some((node.tag_name().name().to_string(), text.to_string()))
                    } else {
                        None
                    }
                })
                .collect::<HashMap<_, _>>())
        } else {
            Err(Error::ParseError(
                "upnp response contains neither `fault` nor `${ACTION}Response` element",
            ))
        }
    }

    async fn make_subscribe_request(
        &self,
        url: &Uri,
        callback: &str,
        timeout_secs: u32,
    ) -> Result<String> {
        let response = Request::builder()
            .uri(self.event_sub_url(url))
            .method("SUBSCRIBE")
            .header("CALLBACK", format!("<{}>", callback))
            .header("NT", "upnp:event")
            .header("TIMEOUT", format!("Second-{}", timeout_secs))
            .body(())
            .unwrap()
            .send_async()
            .await?
            .err_if_not_200()?;

        let sid = response
            .headers()
            .get("sid")
            .ok_or_else(|| Error::MissingHeader("SID"))?
            .to_str()
            .map_err(|_| Error::ParseError("SID header contained non-visible ASCII bytes"))?
            .to_string();

        Ok(sid)
    }

    /// Subscribe for state variable changes.
    ///
    /// It returns the SID which can be used to unsubscribe to the service and a stream of
    /// responses.
    ///
    /// Each response is a [HashMap](std::collections::HashMap) of the state variables.
    ///
    /// # Example usage:
    /// ```rust,no_run
    /// # use async_std::prelude::*;
    /// # async fn subscribe_example() -> Result<(), upnp::Error> {
    /// # let device: upnp::Device = unimplemented!();
    /// # let service: upnp::Service = unimplemented!();
    /// let (_sid, stream) = service.subscribe(device.url(), 300).await?;
    ///
    /// while let Some(state_vars) = stream.next().await {
    ///     for (key, value) in state_vars? {
    ///         println!("{} => {}", key, value);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe(
        &self,
        url: &Uri,
        timeout_secs: u32,
    ) -> Result<(String, impl Stream<Item = Result<HashMap<String, String>>>)> {
        let addr = utils::get_local_addr();
        let listener = TcpListener::bind(addr).await?;

        let addr = format!("http://{}", listener.local_addr().unwrap());

        let sid = self
            .make_subscribe_request(url, &addr, timeout_secs)
            .await?;

        let stream = Gen::new(move |co: Co<Result<_>>| subscribe_stream(listener, co));

        Ok((sid, stream))
    }

    /// Renew a subscription made with the [subscribe](struct.Service.html#method.subscribe) method.
    ///
    /// When the sid is invalid, the control point will respond with a `412 Preconditition failed`.
    pub async fn renew_subscription(&self, url: &Uri, sid: &str, timeout_secs: u32) -> Result<()> {
        Request::builder()
            .uri(self.event_sub_url(url))
            .method("SUBSCRIBE")
            .header("SID", sid)
            .header("TIMEOUT", format!("Second-{}", timeout_secs))
            .body(())
            .unwrap()
            .send_async()
            .await?
            .err_if_not_200()?;

        Ok(())
    }

    /// Unsubscribe from further event notifications.
    ///
    /// The SID is usually obtained by the [subscribe](struct.Service.html#method.subscribe) method.
    ///
    /// When the sid is invalid, the control point will respond with a `412 Preconditition failed`.
    pub async fn unsubscribe(&self, url: &Uri, sid: &str) -> Result<()> {
        Request::builder()
            .uri(self.event_sub_url(url))
            .method("UNSUBSCRIBE")
            .header("SID", sid)
            .body(())
            .unwrap()
            .send_async()
            .await?
            .err_if_not_200()?;

        Ok(())
    }
}

macro_rules! yield_try {
    ( $co:expr => $expr:expr ) => {
        match $expr {
            Ok(val) => val,
            Err(e) => {
                $co.yield_(Err(e.into())).await;
                continue;
            }
        }
    };
}

async fn subscribe_stream(listener: TcpListener, co: Co<Result<HashMap<String, String>>>) {
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let mut lines = BufReader::new(yield_try!(co => stream)).lines();

        let mut input = String::new();
        let mut is_xml = false;

        // sometimes the xml is on one line, sometimes on multiple ones.
        // we dont care about the http stuff before the "<e:propertyset>"
        while let Some(line) = lines.next().await {
            let line = yield_try!(co => line);
            if is_xml || line.starts_with("<e:propertyset") {
                input.push_str(&line);
                is_xml = true;
            }

            if line.ends_with("</e:propertyset>") {
                break;
            };
        }

        let doc = yield_try!(co => Document::parse(&input));
        let hashmap: HashMap<String, String> = doc
            .root_element()
            .children()
            .filter_map(|child| child.first_element_child())
            .filter_map(|node| {
                if let Some(text) = node.text() {
                    Some((node.tag_name().name().to_string(), text.to_string()))
                } else {
                    None
                }
            })
            .collect();

        co.yield_(Ok(hashmap)).await;
    }
}

fn replace_url_path(url: &Uri, path: &PathAndQuery) -> Uri {
    let mut parts = url.clone().into_parts();
    parts.path_and_query = Some(path.clone());
    Uri::from_parts(parts).expect("infallible")
}
