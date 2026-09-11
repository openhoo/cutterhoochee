import { memo, useEffect, useMemo, useState } from "react";
import { AlertTriangle, FileAudio, FileImage, FileVideo, FolderPlus, MoreHorizontal, Plus, RefreshCw, Search, Trash2, Upload, Video } from "lucide-react";

import type { EditorClient, ProjectSnapshot } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { callNative, formatDuration, record, replyPayload, stringValue } from "@/lib/native";

type ThumbnailPreview = {
  scope: string;
  sourceArtifactId: string;
  artifactId?: string;
  url?: string;
  loading: boolean;
  error?: string;
};

const THUMBNAIL_CONCURRENCY = 2;

export const MediaLibrary = memo(function MediaLibrary({
  client,
  snapshot,
  onRefresh,
  onNotice,
  onImport,
  onInsert,
}: {
  client: EditorClient;
  snapshot: ProjectSnapshot;
  onRefresh: () => Promise<void>;
  onNotice: (notice: string) => void;
  onImport: () => void;
  onInsert?: (assetId: string) => void | Promise<void>;
}) {
  const [query, setQuery] = useState("");
  const [busyAsset, setBusyAsset] = useState<string | null>(null);
  const [inspected, setInspected] = useState<string | null>(null);
  const [thumbnails, setThumbnails] = useState<Record<string, ThumbnailPreview>>({});
  const assets = useMemo(() => {
    const normalizedQuery = query.trim().toLowerCase();
    return snapshot.document.assets.filter((asset) => {
      if (!normalizedQuery) return true;
      return asset.original.fileName.toLowerCase().includes(normalizedQuery) || asset.id.toLowerCase().includes(normalizedQuery);
    });
  }, [query, snapshot.document.assets]);
  const thumbnailRequests = useMemo(
    () => snapshot.document.assets.flatMap((asset) => {
      const video = asset.normalization?.video;
      return video?.masterArtifactId && video.frameCount > 0
        ? [{ assetId: asset.id, sourceArtifactId: video.masterArtifactId }]
        : [];
    }),
    [snapshot.document.assets],
  );
  const thumbnailRequestKey = thumbnailRequests.map((request) => `${request.assetId}:${request.sourceArtifactId}`).join("\u001f");
  const context = client.getContext();
  const thumbnailScope = `${context.generation}:${context.projectId ?? ""}:${snapshot.workspaceId}:${snapshot.document.projectId}`;

  useEffect(() => {
    let cancelled = false;
    const requestContext = client.getContext();
    const scope = thumbnailScope;
    const isCurrent = () => {
      const current = client.getContext();
      return !cancelled
        && current.generation === requestContext.generation
        && current.projectId === requestContext.projectId;
    };

    setThumbnails((current) => {
      const next: Record<string, ThumbnailPreview> = {};
      for (const request of thumbnailRequests) {
        const previous = current[request.assetId];
        if (previous?.scope === scope && previous.sourceArtifactId === request.sourceArtifactId) {
          next[request.assetId] = previous;
        }
      }
      return next;
    });

    let cursor = 0;
    const loadThumbnail = async (request: (typeof thumbnailRequests)[number]) => {
      if (!isCurrent()) return;
      setThumbnails((current) => ({
        ...current,
        [request.assetId]: {
          ...current[request.assetId],
          scope,
          sourceArtifactId: request.sourceArtifactId,
          loading: true,
          error: undefined,
        },
      }));
      try {
        const reply = await callNative(client, {
          method: "media",
          params: { action: "thumbnail", assetId: request.assetId, frame: 0 },
        });
        if (!isCurrent()) return;
        const data = replyPayload(reply);
        const artifact = record(data.artifact);
        const artifactId = stringValue(artifact.artifactId || data.artifactId);
        if (!artifactId) throw new Error("The thumbnail response omitted its managed artifact.");
        const url = await client.resolveArtifactUrl(artifactId);
        if (!isCurrent()) return;
        setThumbnails((current) => {
          const previous = current[request.assetId];
          if (!previous || previous.scope !== scope || previous.sourceArtifactId !== request.sourceArtifactId) return current;
          return {
            ...current,
            [request.assetId]: { ...previous, artifactId, url, loading: false, error: undefined },
          };
        });
      } catch (error) {
        if (!isCurrent()) return;
        const message = error instanceof Error && error.message ? error.message : "The generated thumbnail is unavailable.";
        setThumbnails((current) => {
          const previous = current[request.assetId];
          if (!previous || previous.scope !== scope || previous.sourceArtifactId !== request.sourceArtifactId) return current;
          return {
            ...current,
            [request.assetId]: { ...previous, loading: false, url: undefined, error: message },
          };
        });
      }
    };
    const worker = async () => {
      while (isCurrent()) {
        const request = thumbnailRequests[cursor++];
        if (!request) return;
        await loadThumbnail(request);
      }
    };
    const workers = Math.min(THUMBNAIL_CONCURRENCY, thumbnailRequests.length);
    void Promise.all(Array.from({ length: workers }, () => worker()));
    return () => {
      cancelled = true;
    };
  }, [client, thumbnailRequestKey, thumbnailScope]);

  const mediaAction = async (assetId: string, action: "inspect" | "relink" | "remove") => {
    setBusyAsset(assetId);
    try {
      await callNative(client, { method: "media", params: { action, assetId } });
      if (action === "remove") setInspected(null);
      await onRefresh();
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "Media operation failed.");
    } finally {
      setBusyAsset(null);
    }
  };

  return (
    <div className="panel-stack">
      <div className="panel-heading"><div><p className="eyebrow">Library</p><h2>Media</h2></div><Button variant="secondary" size="icon" aria-label="Import media" title="Import media" onClick={onImport}><FolderPlus aria-hidden="true" /></Button></div>
      <div className="search-box"><Search aria-hidden="true" /><input aria-label="Search media" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search media" /></div>
      <div className="media-list" role="list">
        {assets.length === 0 ? <div className="empty-panel"><Upload aria-hidden="true" /><strong>{snapshot.document.assets.length === 0 ? "Nothing imported yet" : "No matching media"}</strong><span>{snapshot.document.assets.length === 0 ? "Import footage, audio, or an image to begin." : "Try a different filename."}</span>{snapshot.document.assets.length === 0 ? <Button variant="secondary" size="sm" onClick={onImport}>Import media</Button> : null}</div> : null}
        {assets.map((asset) => {
          const normalized = asset.normalization;
          const video = normalized?.video;
          const audio = normalized?.audio;
          const ready = Boolean(
            (video?.masterArtifactId && video.frameCount > 0)
            || (audio?.pcmArtifactId && audio.durationFrames > 0),
          );
          const selected = inspected === asset.id;
          const thumbnailRecord = thumbnails[asset.id];
          const thumbnail = thumbnailRecord?.scope === thumbnailScope ? thumbnailRecord : undefined;
          const sourceThumbnailArtifactId = thumbnail?.sourceArtifactId;
          const originalVideo = asset.original.streams.find((stream) => stream.kind === "video");
          const originalAudio = asset.original.streams.find((stream) => stream.kind === "audio");
          const sourceStream = asset.kind === "audio" ? originalAudio : originalVideo ?? originalAudio;
          const sourceDurationMs = sourceStream?.durationMs;
          const width = video?.width ?? originalVideo?.width ?? undefined;
          const height = video?.height ?? originalVideo?.height ?? undefined;
          const dimensions = typeof width === "number" && typeof height === "number" ? `${width}×${height}` : undefined;
          const durationFrames = video?.frameCount ?? audio?.durationFrames;
          const duration = typeof durationFrames === "number"
            ? formatDuration(durationFrames, video?.fpsNum ?? snapshot.document.profile.fpsNum, video?.fpsDen ?? snapshot.document.profile.fpsDen)
            : typeof sourceDurationMs === "number" && Number.isFinite(sourceDurationMs)
              ? formatMilliseconds(sourceDurationMs)
              : undefined;
          const audioFormat = audio && audio.sampleRate > 0 && audio.channels > 0
            ? `${audio.sampleRate / 1000} kHz ${audio.channels === 2 ? "stereo" : `${audio.channels}ch`}`
            : undefined;
          const codec = sourceStream?.codec?.trim() || undefined;
          const metadata = [
            asset.kind === "video" ? "Video" : asset.kind === "audio" ? "Audio" : "Image",
            dimensions,
            duration,
            audioFormat,
            codec,
            typeof asset.original.byteSize === "number" ? formatBytes(asset.original.byteSize) : undefined,
          ].filter((value): value is string => Boolean(value)).join(" · ");
          const thumbnailError = thumbnail?.error;
          return <article
            className={selected ? "media-item selected" : "media-item"}
            role="listitem"
            aria-label={`${asset.original.fileName} · ${metadata}`}
            tabIndex={0}
            key={asset.id}
            draggable
            onDragStart={(event) => { event.dataTransfer.effectAllowed = "copy"; event.dataTransfer.setData("application/x-cutterhoochee-asset", asset.id); }}
            onClick={() => setInspected(asset.id)}
            onKeyDown={(event) => {
              if (event.target !== event.currentTarget) return;
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                setInspected(asset.id);
              }
            }}
          >
            <div className={`asset-thumb ${asset.kind}`} aria-hidden="true">
              {thumbnail?.url ? <img
                src={thumbnail.url}
                alt=""
                draggable={false}
                decoding="async"
                style={{ width: "100%", height: "100%", objectFit: "cover", borderRadius: "6px" }}
                onError={() => {
                  setThumbnails((current) => {
                    const previous = current[asset.id];
                    if (!previous || previous.scope !== thumbnailScope || previous.sourceArtifactId !== sourceThumbnailArtifactId) return current;
                    return {
                      ...current,
                      [asset.id]: { ...previous, loading: false, url: undefined, error: "The generated thumbnail could not be displayed." },
                    };
                  });
                }}
              /> : asset.kind === "video" ? <FileVideo /> : asset.kind === "audio" ? <FileAudio /> : <FileImage />}
              {video ? <span className="asset-duration">{formatDuration(video.frameCount, video.fpsNum, video.fpsDen)}</span> : null}
            </div>
            <div className="media-item-body">
              <div className="media-item-title" title={asset.original.fileName}>{asset.original.fileName}</div>
              <div className="media-item-meta">{metadata}</div>
              {!ready ? <div className="asset-warning" role="status"><AlertTriangle aria-hidden="true" />{normalized ? "Media preparation is incomplete." : "Preparing normalized media…"}</div> : thumbnail?.loading ? <div className="asset-warning" role="status">Generating thumbnail…</div> : thumbnailError ? <div className="asset-warning" title={thumbnailError}><AlertTriangle aria-hidden="true" />Thumbnail unavailable.</div> : null}
            </div>
            {selected ? <div className="asset-actions" onClick={(event) => event.stopPropagation()}>{ready && onInsert ? <Button variant="secondary" size="sm" disabled={busyAsset === asset.id} onClick={() => void onInsert(asset.id)}><Plus aria-hidden="true" />Add to timeline</Button> : null}<Button variant="ghost" size="sm" disabled={busyAsset === asset.id} onClick={() => void mediaAction(asset.id, "inspect")}><Video aria-hidden="true" />Inspect</Button>{ready ? null : <Button variant="ghost" size="sm" disabled={busyAsset === asset.id} onClick={() => void mediaAction(asset.id, "relink")}><RefreshCw aria-hidden="true" />Relink</Button>}<Button variant="ghost" size="sm" disabled={busyAsset === asset.id} onClick={() => void mediaAction(asset.id, "remove")}><Trash2 aria-hidden="true" />Remove</Button></div> : null}
            <button className="icon-button" type="button" aria-label={`Actions for ${asset.original.fileName}`} onClick={(event) => { event.stopPropagation(); setInspected(selected ? null : asset.id); }}><MoreHorizontal aria-hidden="true" /></button>
          </article>;
        })}
      </div>
      <div className="panel-footnote"><span className="status-dot" />Assets are managed inside this project. Originals are never deleted.</div>
    </div>
  );
});

function formatMilliseconds(value: number): string {
  const seconds = Math.max(0, value / 1000);
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds - minutes * 60;
  return `${minutes}:${remainder.toFixed(1).padStart(4, "0")}`;
}

function formatBytes(value: number): string {
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  if (value < 1024 * 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MB`;
  return `${(value / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}
