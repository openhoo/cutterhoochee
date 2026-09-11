import type {
  DestRect,
  RenderLayer,
  RenderPlan,
  RenderSegment,
  RenderTextOverlay,
  RenderTransition,
  RgbaColor,
  SourceRect,
} from "@cutterhoochee/shared";
import type { VideoSnapshot } from "./VideoPool";

type RenderRect = SourceRect | DestRect;

export interface CompositionResult {
  readonly missingVideoKeys: readonly string[];
  readonly missingRasterIds: readonly string[];
}

export type RasterSource = CanvasImageSource;

interface PixelCanvas {
  readonly canvas: HTMLCanvasElement | OffscreenCanvas;
  readonly context: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D;
}
interface TransitionCanvases {
  readonly left: PixelCanvas;
  readonly right: PixelCanvas;
  readonly transition: PixelCanvas;
}

function clampUnit(value: number): number {
  return Math.max(0, Math.min(1, value));
}

function cssColor(color: RgbaColor): string {
  return `rgba(${color.red}, ${color.green}, ${color.blue}, ${clampUnit(color.alpha / 255)})`;
}

function isActive(segment: RenderSegment, frame: number): boolean {
  if (frame < segment.startFrame || frame >= segment.endFrame) return false;
  if (segment.isStillImage) return true;
  return frame >= segment.activeStartFrame && frame < segment.activeEndFrame;
}

function activeTransition(transition: RenderTransition, frame: number): boolean {
  return frame >= transition.startFrame && frame < transition.endFrame;
}

function createPixelCanvas(width: number, height: number): PixelCanvas {
  if (typeof OffscreenCanvas !== "undefined") {
    const canvas = new OffscreenCanvas(width, height);
    const context = canvas.getContext("2d", { alpha: true });
    if (!context) throw new Error("Offscreen canvas 2D context is unavailable.");
    return { canvas, context };
  }
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  const context = canvas.getContext("2d", { alpha: true });
  if (!context) throw new Error("Canvas 2D context is unavailable.");
  return { canvas, context };
}

function clearPixelCanvas(layer: PixelCanvas): void {
  layer.context.setTransform(1, 0, 0, 1, 0, 0);
  layer.context.globalAlpha = 1;
  layer.context.globalCompositeOperation = "copy";
  layer.context.clearRect(0, 0, layer.canvas.width, layer.canvas.height);
  layer.context.globalCompositeOperation = "source-over";
}

function drawImageRect(
  context: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D,
  source: CanvasImageSource,
  sourceRect: RenderRect,
  destination: RenderRect,
  opacity: number,
): void {
  context.save();
  context.globalCompositeOperation = "source-over";
  context.globalAlpha = clampUnit(opacity / 10_000);
  context.imageSmoothingEnabled = true;
  context.imageSmoothingQuality = "high";
  context.drawImage(
    source,
    sourceRect.x,
    sourceRect.y,
    sourceRect.width,
    sourceRect.height,
    destination.x,
    destination.y,
    destination.width,
    destination.height,
  );
  context.restore();
}

function drawRaster(
  context: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D,
  source: RasterSource,
  sourceRect: RenderRect,
  destination: RenderRect,
): void {
  context.save();
  context.globalCompositeOperation = "source-over";
  context.globalAlpha = 1;
  context.imageSmoothingEnabled = true;
  context.imageSmoothingQuality = "high";
  context.drawImage(
    source,
    sourceRect.x,
    sourceRect.y,
    sourceRect.width,
    sourceRect.height,
    destination.x,
    destination.y,
    destination.width,
    destination.height,
  );
  context.restore();
}

/**
 * Composes a moving preview at the immutable project geometry. Transition
 * inputs are first resolved into a transparent premultiplied group and that
 * group is source-over composited once; drawing two globalAlpha images over the
 * already-composed lower layers would incorrectly reveal those layers twice.
 */
export class CanvasComposer {
  private plan: RenderPlan | undefined;
  private layers: readonly RenderLayer[] = [];
  private leftLayer: PixelCanvas | undefined;
  private rightLayer: PixelCanvas | undefined;
  private transitionLayer: PixelCanvas | undefined;
  private transitionOutput: ImageData | undefined;

  constructor(private readonly canvas: HTMLCanvasElement) {}

  setPlan(plan: RenderPlan): void {
    this.plan = plan;
    this.layers = [...plan.layers].sort((left, right) => left.order - right.order);
    if (this.canvas.width !== plan.width || this.canvas.height !== plan.height) {
      this.canvas.width = plan.width;
      this.canvas.height = plan.height;
    }
    this.leftLayer = undefined;
    this.rightLayer = undefined;
    this.transitionLayer = undefined;
    this.transitionOutput = undefined;
  }

  drawFrame(
    frame: number,
    videos: ReadonlyMap<string, VideoSnapshot>,
    rasters: ReadonlyMap<string, RasterSource>,
  ): CompositionResult {
    const plan = this.plan;
    if (!plan) return { missingVideoKeys: [], missingRasterIds: [] };
    if (this.canvas.width !== plan.width || this.canvas.height !== plan.height) {
      this.canvas.width = plan.width;
      this.canvas.height = plan.height;
    }
    const context = this.canvas.getContext("2d", { alpha: false });
    if (!context) throw new Error("Canvas 2D context is unavailable.");
    context.setTransform(1, 0, 0, 1, 0, 0);
    context.globalCompositeOperation = "copy";
    context.globalAlpha = 1;
    context.fillStyle = cssColor(plan.background);
    context.fillRect(0, 0, plan.width, plan.height);
    context.globalCompositeOperation = "source-over";

    const missingVideoKeys: string[] = [];
    const missingRasterIds: string[] = [];
    let overlayWarningDrawn = false;
    for (const layer of this.layers) {
      this.drawLayer(layer, frame, videos, rasters, context, missingVideoKeys, missingRasterIds, () => {
        overlayWarningDrawn = true;
      });
    }
    if (plan.fontWarning && !overlayWarningDrawn) this.drawWarning(context, plan.fontWarning);
    return { missingVideoKeys, missingRasterIds };
  }


  private drawLayer(
    layer: RenderLayer,
    frame: number,
    videos: ReadonlyMap<string, VideoSnapshot>,
    rasters: ReadonlyMap<string, RasterSource>,
    context: CanvasRenderingContext2D,
    missingVideoKeys: string[],
    missingRasterIds: string[],
    markWarning: () => void,
  ): void {
    const activeSegments = layer.segments.filter((segment) => isActive(segment, frame));
    const activeByClipId = new Map(activeSegments.map((segment) => [segment.clipId, segment]));
    const drawn = new Set<string>();
    for (const transition of layer.transitions) {
      if (!activeTransition(transition, frame)) continue;
      const left = activeByClipId.get(transition.leftClipId);
      const right = activeByClipId.get(transition.rightClipId);
      if (!left || !right) continue;
      drawn.add(left.clipId);
      drawn.add(right.clipId);
      const leftSnapshot = videos.get(left.clipId);
      const rightSnapshot = videos.get(right.clipId);
      if (!leftSnapshot) missingVideoKeys.push(left.clipId);
      if (!rightSnapshot) missingVideoKeys.push(right.clipId);
      if (leftSnapshot && rightSnapshot) {
        this.drawTransition(context, left, right, leftSnapshot.source, rightSnapshot.source, transition, frame);
      }
    }
    for (const segment of activeSegments) {
      if (drawn.has(segment.clipId)) continue;
      const snapshot = videos.get(segment.clipId);
      if (!snapshot) {
        missingVideoKeys.push(segment.clipId);
        continue;
      }
      drawImageRect(context, snapshot.source, segment.sourceRect, segment.destRect, segment.opacity);
    }
    for (const overlay of layer.textOverlays) {
      if (frame < overlay.startFrame || frame >= overlay.endFrame) continue;
      const source = rasters.get(overlay.rasterArtifactId);
      if (!source) {
        missingRasterIds.push(overlay.rasterArtifactId);
        continue;
      }
      drawRaster(context, source, overlay.rasterSourceRect, overlay.rasterDestRect);
      if (overlay.fontWarning) {
        this.drawWarning(context, overlay.fontWarning);
        markWarning();
      }
    }
  }

  private drawTransition(
    context: CanvasRenderingContext2D,
    left: RenderSegment,
    right: RenderSegment,
    leftSource: CanvasImageSource,
    rightSource: CanvasImageSource,
    transition: RenderTransition,
    frame: number,
  ): void {
    const width = this.canvas.width;
    const height = this.canvas.height;
    const existingLeft = this.leftLayer;
    const existingRight = this.rightLayer;
    const existingTransition = this.transitionLayer;
    const canvases: TransitionCanvases =
      existingLeft &&
      existingRight &&
      existingTransition &&
      existingLeft.canvas.width === width &&
      existingLeft.canvas.height === height &&
      existingRight.canvas.width === width &&
      existingRight.canvas.height === height &&
      existingTransition.canvas.width === width &&
      existingTransition.canvas.height === height
        ? { left: existingLeft, right: existingRight, transition: existingTransition }
        : {
            left: createPixelCanvas(width, height),
            right: createPixelCanvas(width, height),
            transition: createPixelCanvas(width, height),
          };
    this.leftLayer = canvases.left;
    this.rightLayer = canvases.right;
    this.transitionLayer = canvases.transition;
    const { left: leftLayer, right: rightLayer, transition: transitionLayer } = canvases;
    clearPixelCanvas(leftLayer);
    clearPixelCanvas(rightLayer);
    clearPixelCanvas(transitionLayer);
    drawImageRect(leftLayer.context, leftSource, left.sourceRect, left.destRect, left.opacity);
    drawImageRect(rightLayer.context, rightSource, right.sourceRect, right.destRect, right.opacity);
    const leftPixels = leftLayer.context.getImageData(0, 0, width, height);
    const rightPixels = rightLayer.context.getImageData(0, 0, width, height);
    const output = this.transitionOutput && this.transitionOutput.width === width && this.transitionOutput.height === height
      ? this.transitionOutput
      : transitionLayer.context.createImageData(width, height);
    this.transitionOutput = output;
    const progress = clampUnit((frame - transition.startFrame) / transition.durationFrames);
    const leftWeight = 1 - progress;
    const rightWeight = progress;
    for (let index = 0; index < output.data.length; index += 4) {
      const leftAlpha = leftPixels.data[index + 3] / 255;
      const rightAlpha = rightPixels.data[index + 3] / 255;
      const alpha = leftAlpha * leftWeight + rightAlpha * rightWeight;
      output.data[index + 3] = Math.round(alpha * 255);
      if (alpha <= 0) {
        output.data[index] = 0;
        output.data[index + 1] = 0;
        output.data[index + 2] = 0;
        continue;
      }
      const red = (leftPixels.data[index] * leftAlpha * leftWeight + rightPixels.data[index] * rightAlpha * rightWeight) / alpha;
      const green = (leftPixels.data[index + 1] * leftAlpha * leftWeight + rightPixels.data[index + 1] * rightAlpha * rightWeight) / alpha;
      const blue = (leftPixels.data[index + 2] * leftAlpha * leftWeight + rightPixels.data[index + 2] * rightAlpha * rightWeight) / alpha;
      output.data[index] = Math.round(Math.max(0, Math.min(255, red)));
      output.data[index + 1] = Math.round(Math.max(0, Math.min(255, green)));
      output.data[index + 2] = Math.round(Math.max(0, Math.min(255, blue)));
    }
    transitionLayer.context.putImageData(output, 0, 0);
    context.save();
    context.globalCompositeOperation = "source-over";
    context.globalAlpha = 1;
    context.drawImage(transitionLayer.canvas, 0, 0);
    context.restore();
  }

  private drawWarning(context: CanvasRenderingContext2D, message: string): void {
    context.save();
    context.fillStyle = "rgba(228, 176, 96, 0.92)";
    context.fillRect(12, 12, 24, 24);
    context.fillStyle = "#111315";
    context.font = "bold 18px sans-serif";
    context.textAlign = "center";
    context.textBaseline = "middle";
    context.fillText("!", 24, 24);
    context.textAlign = "left";
    context.font = "11px sans-serif";
    context.fillStyle = "#F1F0EB";
    context.fillText(message.slice(0, 120), 44, 25);
    context.restore();
  }

  dispose(): void {
    this.plan = undefined;
    this.layers = [];
    this.leftLayer = undefined;
    this.rightLayer = undefined;
    this.transitionLayer = undefined;
    this.transitionOutput = undefined;
  }
}
