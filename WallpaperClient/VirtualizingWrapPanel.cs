using System.Windows;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Media;
using System.Windows.Threading;
using Point = System.Windows.Point;
using Size = System.Windows.Size;

namespace WallpaperClient;

/// <summary>
/// A vertically scrolling wrap panel that realizes only the visible rows.
/// Card dimensions are fixed deliberately: predictable geometry keeps
/// virtualization stable while the number of columns adapts to the window.
/// </summary>
internal sealed class VirtualizingWrapPanel : VirtualizingPanel, IScrollInfo
{
    public static readonly DependencyProperty ItemWidthProperty = DependencyProperty.Register(
        nameof(ItemWidth), typeof(double), typeof(VirtualizingWrapPanel),
        new FrameworkPropertyMetadata(188d, FrameworkPropertyMetadataOptions.AffectsMeasure));

    public static readonly DependencyProperty ItemHeightProperty = DependencyProperty.Register(
        nameof(ItemHeight), typeof(double), typeof(VirtualizingWrapPanel),
        new FrameworkPropertyMetadata(184d, FrameworkPropertyMetadataOptions.AffectsMeasure));

    private Size _extent;
    private Size _viewport;
    private Point _offset;
    private int _itemsPerRow = 1;

    public double ItemWidth
    {
        get => (double)GetValue(ItemWidthProperty);
        set => SetValue(ItemWidthProperty, value);
    }

    public double ItemHeight
    {
        get => (double)GetValue(ItemHeightProperty);
        set => SetValue(ItemHeightProperty, value);
    }

    protected override Size MeasureOverride(Size availableSize)
    {
        var owner = ItemsControl.GetItemsOwner(this);
        var itemCount = owner?.Items.Count ?? 0;
        var viewportWidth = double.IsInfinity(availableSize.Width)
            ? Math.Max(ItemWidth, ActualWidth)
            : Math.Max(0, availableSize.Width);
        var viewportHeight = double.IsInfinity(availableSize.Height)
            ? Math.Max(ItemHeight, ActualHeight)
            : Math.Max(0, availableSize.Height);

        _itemsPerRow = Math.Max(1, (int)Math.Floor(viewportWidth / Math.Max(1, ItemWidth)));
        var rowCount = itemCount == 0 ? 0 : (itemCount + _itemsPerRow - 1) / _itemsPerRow;
        UpdateScrollInfo(
            new Size(viewportWidth, rowCount * ItemHeight),
            new Size(viewportWidth, viewportHeight));

        // WPF may measure the panel once before ItemsControl connects its
        // generator. Defer realization instead of crashing during startup.
        if (ItemContainerGenerator is null)
        {
            Dispatcher.BeginInvoke(DispatcherPriority.Loaded, InvalidateMeasure);
            return availableSize;
        }

        if (itemCount == 0 || viewportHeight <= 0)
        {
            RemoveRealizedChildren(0, -1);
            return availableSize;
        }

        var firstRow = Math.Max(0, (int)Math.Floor(VerticalOffset / ItemHeight));
        var lastRow = Math.Min(
            rowCount - 1,
            (int)Math.Ceiling((VerticalOffset + viewportHeight) / ItemHeight));
        var firstIndex = Math.Min(itemCount - 1, firstRow * _itemsPerRow);
        var lastIndex = Math.Min(itemCount - 1, ((lastRow + 1) * _itemsPerRow) - 1);

        RealizeRange(firstIndex, lastIndex);
        RemoveRealizedChildren(firstIndex, lastIndex);
        return availableSize;
    }

    protected override Size ArrangeOverride(Size finalSize)
    {
        var generator = ItemContainerGenerator;
        for (var childIndex = 0; childIndex < InternalChildren.Count; childIndex++)
        {
            var child = InternalChildren[childIndex];
            var itemIndex = generator.IndexFromGeneratorPosition(new GeneratorPosition(childIndex, 0));
            if (itemIndex < 0) continue;
            var row = itemIndex / _itemsPerRow;
            var column = itemIndex % _itemsPerRow;
            child.Arrange(new Rect(
                column * ItemWidth,
                (row * ItemHeight) - VerticalOffset,
                ItemWidth,
                ItemHeight));
        }
        return finalSize;
    }

    protected override void BringIndexIntoView(int index)
    {
        if (index < 0) return;
        var top = (index / _itemsPerRow) * ItemHeight;
        if (top < VerticalOffset) SetVerticalOffset(top);
        else if (top + ItemHeight > VerticalOffset + ViewportHeight)
            SetVerticalOffset(top + ItemHeight - ViewportHeight);
    }

    private void RealizeRange(int firstIndex, int lastIndex)
    {
        var generator = ItemContainerGenerator;
        var start = generator.GeneratorPositionFromIndex(firstIndex);
        var childIndex = start.Offset == 0 ? start.Index : start.Index + 1;

        using var generation = generator.StartAt(start, GeneratorDirection.Forward, true);
        for (var itemIndex = firstIndex; itemIndex <= lastIndex; itemIndex++, childIndex++)
        {
            var child = (UIElement)generator.GenerateNext(out var newlyRealized);
            if (newlyRealized)
            {
                if (childIndex >= InternalChildren.Count) AddInternalChild(child);
                else InsertInternalChild(childIndex, child);
                generator.PrepareItemContainer(child);
            }
            child.Measure(new Size(ItemWidth, ItemHeight));
        }
    }

    private void RemoveRealizedChildren(int firstIndex, int lastIndex)
    {
        var generator = ItemContainerGenerator;
        for (var childIndex = InternalChildren.Count - 1; childIndex >= 0; childIndex--)
        {
            var position = new GeneratorPosition(childIndex, 0);
            var itemIndex = generator.IndexFromGeneratorPosition(position);
            if (itemIndex >= firstIndex && itemIndex <= lastIndex) continue;

            if (generator is IRecyclingItemContainerGenerator recycling)
                recycling.Recycle(position, 1);
            else
                generator.Remove(position, 1);
            RemoveInternalChildRange(childIndex, 1);
        }
    }

    private void UpdateScrollInfo(Size extent, Size viewport)
    {
        var changed = !AreClose(_extent, extent) || !AreClose(_viewport, viewport);
        _extent = extent;
        _viewport = viewport;
        SetVerticalOffset(_offset.Y);
        if (changed) ScrollOwner?.InvalidateScrollInfo();
    }

    private static bool AreClose(Size left, Size right) =>
        Math.Abs(left.Width - right.Width) < 0.5 && Math.Abs(left.Height - right.Height) < 0.5;

    public ScrollViewer? ScrollOwner { get; set; }
    public bool CanHorizontallyScroll { get; set; }
    public bool CanVerticallyScroll { get; set; } = true;
    public double ExtentWidth => _extent.Width;
    public double ExtentHeight => _extent.Height;
    public double ViewportWidth => _viewport.Width;
    public double ViewportHeight => _viewport.Height;
    public double HorizontalOffset => _offset.X;
    public double VerticalOffset => _offset.Y;

    public void LineUp() => SetVerticalOffset(VerticalOffset - 24);
    public void LineDown() => SetVerticalOffset(VerticalOffset + 24);
    public void LineLeft() { }
    public void LineRight() { }
    public void MouseWheelUp() => SetVerticalOffset(VerticalOffset - (ItemHeight * 0.75));
    public void MouseWheelDown() => SetVerticalOffset(VerticalOffset + (ItemHeight * 0.75));
    public void MouseWheelLeft() { }
    public void MouseWheelRight() { }
    public void PageUp() => SetVerticalOffset(VerticalOffset - ViewportHeight);
    public void PageDown() => SetVerticalOffset(VerticalOffset + ViewportHeight);
    public void PageLeft() { }
    public void PageRight() { }
    public void SetHorizontalOffset(double offset) { }

    public void SetVerticalOffset(double offset)
    {
        var maximum = Math.Max(0, ExtentHeight - ViewportHeight);
        var coerced = Math.Clamp(double.IsNaN(offset) ? 0 : offset, 0, maximum);
        if (Math.Abs(coerced - _offset.Y) < 0.1) return;
        _offset.Y = coerced;
        ScrollOwner?.InvalidateScrollInfo();
        InvalidateMeasure();
    }

    public Rect MakeVisible(Visual visual, Rect rectangle)
    {
        DependencyObject? container = visual;
        while (container is not null && VisualTreeHelper.GetParent(container) != this)
            container = VisualTreeHelper.GetParent(container);
        if (container is UIElement element)
        {
            var childIndex = InternalChildren.IndexOf(element);
            var index = childIndex < 0
                ? -1
                : ItemContainerGenerator.IndexFromGeneratorPosition(new GeneratorPosition(childIndex, 0));
            if (index >= 0) BringIndexIntoView(index);
        }
        return rectangle;
    }
}
