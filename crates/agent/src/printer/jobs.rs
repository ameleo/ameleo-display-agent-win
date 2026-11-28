// SPDX-License-Identifier: GPL-3.0-only

//! The print jobs of the client printers. The Print Support App's task
//! receives each job printed to one of its virtual printers (OpenXPS and
//! its PrintTicket) and hands it to the agent over the link
//! (`print_link::Request::Job`, see [`super::link_server`]). The agent puts
//! the PrintTicket for the client driver ([`super::ticket`]) in the document
//! in place of the guest's (`Metadata/Job_PT.xml`) and hands it to the host
//! (`Host.Printer1.BeginJob`, `WriteJob`..., `EndJob`).

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use proto::HostPrinterProxy;
use proto::printer::CHUNK_MAX;
use zbus::proxy::CacheProperties;

use super::Printers;
use crate::host::Host;

/// The host may forward to its client.
const HOST_TIMEOUT: Duration = Duration::from_secs(60);
/// Larger jobs are refused.
pub const MAX_JOB: usize = 1024 * 1024 * 1024;
const FORMAT: &str = "application/oxps";

static NEXT_JOB: AtomicU32 = AtomicU32::new(1);

/// Calls `Host.Printer1` from a blocking thread and waits for the answer.
pub fn call_host<F, Fut>(host: &Host, what: String, call: F) -> Result<(), String>
where
    F: FnOnce(HostPrinterProxy<'static>) -> Fut + Send + 'static,
    Fut: Future<Output = zbus::Result<()>> + Send + 'static,
{
    let rx = host.start(what, move |conn| async move {
        let s = |e: zbus::Error| e.to_string();
        let p = HostPrinterProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(s)?;
        call(p).await.map_err(s)?;
        Ok((String::new(), Vec::new()))
    })?;
    rx.recv_timeout(HOST_TIMEOUT)
        .map_err(|_| "host did not answer".to_string())?
        .map(|_| ())
}

/// A job printed to the virtual printer `uri`.
pub struct Job {
    pub uri: String,
    pub title: String,
    pub user: String,
    /// The job's PrintTicket, from the virtual printer.
    pub ticket: String,
    pub data: Vec<u8>,
}

/// The job to the host.
pub fn deliver(printers: &Printers, host: &Host, job: Job) -> Result<(), String> {
    let started = Instant::now();
    let Some((printer, queue)) = printers.by_uri(&job.uri) else {
        return Err(format!("{}: the client printer is gone", job.uri));
    };
    let ticket = super::ticket::for_client(&job.ticket).unwrap_or_default();
    let mut data = job.data;
    // The client prints with the PrintTicket of the document: the client's.
    if ticket.is_empty() {
        tracing::warn!(
            printer,
            queue,
            "print job without a PrintTicket: the client prints with its defaults"
        );
    } else {
        match super::xps::with_job_ticket(&data, &ticket) {
            Ok(rewritten) => data = rewritten,
            Err(e) => tracing::info!(
                printer,
                queue,
                "print job sent with its own PrintTicket: {e}"
            ),
        }
    }
    let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let (standard, own) = super::ticket::summary(&ticket);
    let bytes = data.len();
    let name = job.title;
    match send(host, printer, id, name.clone(), job.user, ticket, data) {
        Ok(()) => {
            tracing::info!(printer, job = id, name, bytes, ?standard, ?own, took = ?started.elapsed(), "print job sent to the host");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(printer, job = id, name, bytes, "print job failed: {e}");
            Err(e)
        }
    }
}

fn send(
    host: &Host,
    printer: u32,
    job: u32,
    name: String,
    user: String,
    ticket: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let what = format!(
        "BeginJob printer={printer} job={job} ticket={}",
        ticket.len()
    );
    call_host(host, what, move |p| async move {
        p.begin_job(printer, job, FORMAT, &name, &user, &ticket)
            .await
    })?;
    let res = (|| {
        for chunk in data.chunks(CHUNK_MAX) {
            let chunk = chunk.to_vec();
            call_host(host, format!("WriteJob job={job}"), move |p| async move {
                p.write_job(job, chunk).await
            })?;
        }
        call_host(host, format!("EndJob job={job}"), move |p| async move {
            p.end_job(job).await
        })
    })();
    if res.is_err() {
        let _ = call_host(host, format!("AbortJob job={job}"), move |p| async move {
            p.abort_job(job).await
        });
    }
    res
}
